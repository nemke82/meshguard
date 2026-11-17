/*
 * meshguard-limits: per-client limits for the MeshGuard relay (ADR-007).
 *
 *  - Subscriptions: at most max_subscriptions per client, counted by the
 *    broker itself (mosquitto_client_sub_count), so expired sessions, clean
 *    starts and deleted accounts need no bookkeeping here. A SUBSCRIBE
 *    beyond that is refused (Mosquitto then disconnects the client).
 *  - Publish rate: a token bucket per client id, BURST publishes, refilled at
 *    RATE per second. Excess publishes get QUOTA_EXCEEDED. The bucket
 *    survives reconnects; entries idle for an hour (full again by then) are
 *    dropped, so memory follows active clients only.
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

struct client_entry {
	char *id;
	double tokens;
	double last;
	UT_hash_handle hh;
};

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
	HASH_ADD_KEYPTR(hh, clients, e->id, strlen(e->id), e);
	return e;
}

static int on_subscribe(int event, void *event_data, void *userdata)
{
	struct mosquitto_evt_subscribe *ed = event_data;
	(void)event;
	(void)userdata;

	if(mosquitto_client_sub_count(ed->client) >= (int)max_subs){
		mosquitto_log_printf(MOSQ_LOG_WARNING, "meshguard-limits: subscription limit reached");
		return MOSQ_ERR_ACL_DENIED;
	}
	return MOSQ_ERR_SUCCESS;
}

/* Drop rate-limit entries idle for an hour. */
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
		if(t - e->last > 3600.0){
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
		HASH_DEL(clients, e);
		free(e->id);
		free(e);
	}
	return MOSQ_ERR_SUCCESS;
}
