#!/usr/bin/env bash

set -exo pipefail

## init varibles
USER="horae"
DATA_DIR="/home/${USER}/data"
DATA_PATH="${DATA_DIR}/horaedb"
CONFIG_FILE="/etc/horaedb/horaedb.toml"

# enable jemalloc heap profiling
export MALLOC_CONF="prof:true,prof_active:false,lg_prof_sample:19"

## data dir
mkdir -p ${DATA_DIR}
chmod +777 -R ${DATA_DIR}
chown -R ${USER}.${USER} ${DATA_DIR}

exec /usr/bin/horaedb-server --config ${CONFIG_FILE}
