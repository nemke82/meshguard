/*
 * meshguard-limits: per-client limits for the MeshGuard relay (ADR-007).
 *
 *  - Subscriptions: at most max_subscriptions distinct filters per client.
 *    Re-subscribing to a filter the client already has is always allowed
 *    (clients resubscribe after every reconnect); only a new filter beyond
 *    the limit is refused (Mosquitto then disconnects the client). The
 *    filters known per client are reset whenever a client connects without
 *    any subscriptions left in its session (new session, clean start,
 *    expired), so they follow the broker's own state.
 *  - Publish rate: a token bucket per client id, BURST publishes, refilled at
 *    RATE per second. Excess publishes get QUOTA_EXCEEDED. The bucket
 *    survives reconnects; entries of clients idle longer than any session
 *    can live are dropped, so memory follows real clients only.
 *
 * The 24-hour message lifetime cap is a small patch to the broker
 * (0001-meshguard-clamp-message-expiry.patch): a plugin cannot see or change
 * the expiry of an incoming PUBLISH in Mosquitto 2.1.
 *
 * Options (all optional): plugin_opt_max_subscriptions, plugin_opt_burst,
 * plugin_opt_rate_per_sec.
 *
 * SPDX-License-Identifier: MIT
 */
#include <stdlib.h>
#include <string.h>
#include <time.h>

#include "mosquitto.h"
#include "uthash.h"

#define PLUGIN_NAME "meshguard-limits"
#define PLUGIN_VERSION "1.0"

MOSQUITTO_PLUGIN_DECLARE_VERSION(5);

struct sub_entry {
	char *filter;
	UT_hash_handle hh;
};

struct client_entry {
	char *id;
	struct sub_entry *subs;
	double tokens;
	double last;      /* last publish (rate bucket) */
	double seen;      /* last activity of any kind */
	UT_hash_handle hh;
};

/* Longer than any session can live (persistent_client_expiration 3d). */
#define IDLE_DROP_S (4.0 * 24 * 3600)

static void clear_subs(struct client_entry *e)
{
	struct sub_entry *s, *stmp;
	HASH_ITER(hh, e->subs, s, stmp){
		HASH_DEL(e->subs, s);
		free(s->filter);
		free(s);
	}
}

static mosquitto_plugin_id_t *pid = NULL;
static struct client_entry *clients = NULL;
static unsigned int max_subs = 64;
static double burst = 20.0;
static double rate = 1.0;

static double now_s(void)
{
	struct timespec ts;
	clock_gettime(CLOCK_MONOTONIC, &ts);
	return (double)ts.tv_sec + (double)ts.tv_nsec / 1e9;
}

static struct client_entry *entry_for(struct mosquitto *client)
{
	const char *id = mosquitto_client_id(client);
	struct client_entry *e = NULL;

	if(id == NULL){
		return NULL;
	}
	HASH_FIND_STR(clients, id, e);
	if(e){
		return e;
	}
	e = calloc(1, sizeof(*e));
	if(e == NULL){
		return NULL;
	}
	e->id = strdup(id);
	if(e->id == NULL){
		free(e);
		return NULL;
	}
	e->tokens = burst;
	e->last = now_s();
	e->seen = e->last;
	HASH_ADD_KEYPTR(hh, clients, e->id, strlen(e->id), e);
	return e;
}

static int on_connect(int event, void *event_data, void *userdata)
{
	struct mosquitto_evt_connect *ed = event_data;
	struct client_entry *e;
	(void)event;
	(void)userdata;

	e = entry_for(ed->client);
	if(e == NULL){
		return MOSQ_ERR_SUCCESS;
	}
	e->seen = now_s();
	if(mosquitto_client_sub_count(ed->client) == 0){
		clear_subs(e); /* nothing left in the session */
	}
	return MOSQ_ERR_SUCCESS;
}

static int on_subscribe(int event, void *event_data, void *userdata)
{
	struct mosquitto_evt_subscribe *ed = event_data;
	struct client_entry *e;
	struct sub_entry *s = NULL;
	(void)event;
	(void)userdata;

	e = entry_for(ed->client);
	if(e == NULL || ed->data.topic_filter == NULL){
		return MOSQ_ERR_NOMEM;
	}
	e->seen = now_s();
	HASH_FIND_STR(e->subs, ed->data.topic_filter, s);
	if(s){
		return MOSQ_ERR_SUCCESS; /* already subscribed: always allowed */
	}
	if(HASH_COUNT(e->subs) >= max_subs
			|| mosquitto_client_sub_count(ed->client) >= (int)max_subs){
		mosquitto_log_printf(MOSQ_LOG_WARNING, "meshguard-limits: subscription limit reached");
		return MOSQ_ERR_ACL_DENIED;
	}
	s = calloc(1, sizeof(*s));
	if(s == NULL){
		return MOSQ_ERR_NOMEM;
	}
	s->filter = strdup(ed->data.topic_filter);
	if(s->filter == NULL){
		free(s);
		return MOSQ_ERR_NOMEM;
	}
	HASH_ADD_KEYPTR(hh, e->subs, s->filter, strlen(s->filter), s);
	return MOSQ_ERR_SUCCESS;
}

static int on_unsubscribe(int event, void *event_data, void *userdata)
{
	struct mosquitto_evt_unsubscribe *ed = event_data;
	struct client_entry *e;
	struct sub_entry *s = NULL;
	(void)event;
	(void)userdata;

	e = entry_for(ed->client);
	if(e == NULL || ed->data.topic_filter == NULL){
		return MOSQ_ERR_SUCCESS;
	}
	HASH_FIND_STR(e->subs, ed->data.topic_filter, s);
	if(s){
		HASH_DEL(e->subs, s);
		free(s->filter);
		free(s);
	}
	return MOSQ_ERR_SUCCESS;
}

/* Drop entries of clients idle longer than any session can live. */
static int on_tick(int event, void *event_data, void *userdata)
{
	static double last_sweep = 0.0;
	struct client_entry *e, *etmp;
	double t = now_s();
	(void)event;
	(void)event_data;
	(void)userdata;

	if(t - last_sweep < 60.0){
		return MOSQ_ERR_SUCCESS;
	}
	last_sweep = t;
	HASH_ITER(hh, clients, e, etmp){
		if(t - e->seen > IDLE_DROP_S && t - e->last > IDLE_DROP_S){
			clear_subs(e);
			HASH_DEL(clients, e);
			free(e->id);
			free(e);
		}
	}
	return MOSQ_ERR_SUCCESS;
}

static int on_message_in(int event, void *event_data, void *userdata)
{
	struct mosquitto_evt_message *ed = event_data;
	struct client_entry *e;
	double t;
	(void)event;
	(void)userdata;

	if(ed->client == NULL){
		return MOSQ_ERR_SUCCESS; /* broker-internal */
	}
	e = entry_for(ed->client);
	if(e == NULL){
		return MOSQ_ERR_NOMEM;
	}
	t = now_s();
	e->tokens += (t - e->last) * rate;
	if(e->tokens > burst){
		e->tokens = burst;
	}
	e->last = t;
	if(e->tokens < 1.0){
		return MOSQ_ERR_QUOTA_EXCEEDED;
	}
	e->tokens -= 1.0;
	e->seen = t;
	return MOSQ_ERR_SUCCESS;
}

int mosquitto_plugin_init(mosquitto_plugin_id_t *identifier, void **user_data,
		struct mosquitto_opt *opts, int opt_count)
{
	int rc;
	(void)user_data;

	for(int i = 0; i < opt_count; i++){
		if(!strcmp(opts[i].key, "max_subscriptions")){
			max_subs = (unsigned int)strtoul(opts[i].value, NULL, 10);
		}else if(!strcmp(opts[i].key, "burst")){
			burst = strtod(opts[i].value, NULL);
		}else if(!strcmp(opts[i].key, "rate_per_sec")){
			rate = strtod(opts[i].value, NULL);
		}
	}
	pid = identifier;
	mosquitto_plugin_set_info(identifier, PLUGIN_NAME, PLUGIN_VERSION);
	rc = mosquitto_callback_register(pid, MOSQ_EVT_SUBSCRIBE, on_subscribe, NULL, NULL);
	if(rc == MOSQ_ERR_SUCCESS){
		rc = mosquitto_callback_register(pid, MOSQ_EVT_UNSUBSCRIBE, on_unsubscribe, NULL, NULL);
	}
	if(rc == MOSQ_ERR_SUCCESS){
		rc = mosquitto_callback_register(pid, MOSQ_EVT_CONNECT, on_connect, NULL, NULL);
	}
	if(rc == MOSQ_ERR_SUCCESS){
		rc = mosquitto_callback_register(pid, MOSQ_EVT_TICK, on_tick, NULL, NULL);
	}
	if(rc == MOSQ_ERR_SUCCESS){
		rc = mosquitto_callback_register(pid, MOSQ_EVT_MESSAGE_IN, on_message_in, NULL, NULL);
	}
	return rc;
}

int mosquitto_plugin_cleanup(void *user_data, struct mosquitto_opt *opts, int opt_count)
{
	struct client_entry *e, *etmp;
	(void)user_data;
	(void)opts;
	(void)opt_count;

	HASH_ITER(hh, clients, e, etmp){
		clear_subs(e);
		HASH_DEL(clients, e);
		free(e->id);
		free(e);
	}
	return MOSQ_ERR_SUCCESS;
}
