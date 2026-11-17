# Mosquitto for the MeshGuard relay (ADR-007), built from the release
# tarball with one small patch (24-hour message lifetime cap) and the
# meshguard-limits plugin. Pinned versions; the tarball is checked by hash.
FROM docker.io/library/alpine:3.22 AS build
RUN apk add --no-cache build-base cmake openssl-dev cjson-dev linux-headers curl
ARG MOSQUITTO_VERSION=2.1.2
ARG MOSQUITTO_SHA256=fd905380691ac65ea5a93779e8214941829e3d6e038d5edff9eac5fd74cbed02
WORKDIR /build
RUN curl -fsSLo mosquitto.tar.gz "https://mosquitto.org/files/source/mosquitto-${MOSQUITTO_VERSION}.tar.gz" \
 && echo "${MOSQUITTO_SHA256}  mosquitto.tar.gz" | sha256sum -c - \
 && tar xzf mosquitto.tar.gz
COPY 0001-meshguard-clamp-message-expiry.patch meshguard_limits.c /build/
RUN cd "mosquitto-${MOSQUITTO_VERSION}" \
 && patch -p1 < /build/0001-meshguard-clamp-message-expiry.patch \
 && cmake -S . -B out -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX=/opt/mosquitto \
      -DWITH_DOCS=OFF -DWITH_TESTS=OFF -DWITH_CLIENTS=OFF -DWITH_APPS=OFF \
      -DWITH_WEBSOCKETS=OFF -DWITH_SOCKS=OFF -DWITH_TLS_PSK=OFF -DWITH_CTRL_SHELL=OFF \
      -DWITH_BUNDLED_DEPS=ON -DWITH_PLUGIN_PERSIST_SQLITE=OFF -DWITH_PLUGIN_EXAMPLES=OFF \
      -DWITH_PLUGIN_SPARKPLUG_AWARE=OFF -DWITH_PLUGIN_PASSWORD_FILE=OFF -DWITH_PLUGIN_ACL_FILE=OFF \
 && cmake --build out -j"$(nproc)" \
 && cmake --install out \
 && cc -shared -fPIC -O2 -Wall -Wextra -Werror \
      -Iinclude -Ideps -o /opt/mosquitto/lib/meshguard_limits.so /build/meshguard_limits.c

FROM docker.io/library/alpine:3.22
RUN apk add --no-cache openssl cjson \
 && addgroup -S -g 1883 mosquitto \
 && adduser -S -D -H -u 1883 -G mosquitto mosquitto \
 && mkdir -p /mosquitto/config /mosquitto/data /mosquitto/certs \
 && chown mosquitto:mosquitto /mosquitto/data
COPY --from=build /opt/mosquitto /opt/mosquitto
ENV LD_LIBRARY_PATH=/opt/mosquitto/lib
USER mosquitto
EXPOSE 8883
ENTRYPOINT ["/opt/mosquitto/sbin/mosquitto", "-c", "/mosquitto/config/mosquitto.conf"]
