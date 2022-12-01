#!/bin/bash

# This script sets up 2 Namada chains locally
# `make build` and `make build-wasm-scripts` on Namada directory in advance
# Run with `setup-namada.sh ${namada_dir}`

set -e

NAMADA_DIR=$1
if [ -z "${NAMADA_DIR}" ]
then
  echo "ERROR: Namada directory should be given"
  exit 1
fi
cd $(dirname $0)
IBC_RS_DIR=${PWD%/scripts*}

# edit for your environment
NAMADAC="${NAMADA_DIR}/target/debug/namadac"
NAMADAN="${NAMADA_DIR}/target/debug/namadan"
NAMADAW="${NAMADA_DIR}/target/debug/namadaw"
GENESIS_PATH_A="${NAMADA_DIR}/genesis/e2e-tests-single-node.toml"
GENESIS_PATH_B="${NAMADA_DIR}/genesis/e2e-tests-single-node-b.toml"
CHECKSUM_PATH="${NAMADA_DIR}/wasm/checksums.json"
DATA_DIR="${IBC_RS_DIR}/data"

NET_ADDR_A="127.0.0.1:27656"
NET_ADDR_B="127.0.0.1:28656"
LEDGER_ADDR_A="127.0.0.1:27657"
LEDGER_ADDR_B="127.0.0.1:28657"

HERMES_CONFIG_TEMPLATE="
[global]
log_level = 'debug'

[mode]

[mode.clients]
enabled = true
refresh = true
misbehaviour = true

[mode.connections]
enabled = false

[mode.channels]
enabled = false

[mode.packets]
enabled = true
clear_interval = 10
clear_on_start = false
tx_confirmation = true

[telemetry]
enabled = false
host = '127.0.0.1'
port = 3001

[[chains]]
id = '_CHAIN_ID_A_'
rpc_addr = 'http://127.0.0.1:27657'
grpc_addr = 'http://127.0.0.1:9090'
websocket_addr = 'ws://127.0.0.1:27657/websocket'
rpc_timeout = '10s'
account_prefix = 'cosmos'
key_name = 'relayer'
store_prefix = 'ibc'
max_gas = 3000000
max_msg_num = 30
max_tx_size = 2097152
gas_price = { price = 0.001, denom = 'stake' }
clock_drift = '5s'
trusting_period = '14days'
trust_threshold = { numerator = '1', denominator = '3' }

[[chains]]
id = '_CHAIN_ID_B_'
rpc_addr = 'http://127.0.0.1:28657'
grpc_addr = 'http://127.0.0.1:9090'
websocket_addr = 'ws://127.0.0.1:28657/websocket'
rpc_timeout = '10s'
account_prefix = 'cosmos'
key_name = 'relayer'
store_prefix = 'ibc'
max_gas = 3000000
max_msg_num = 30
max_tx_size = 2097152
gas_price = { price = 0.001, denom = 'stake' }
clock_drift = '5s'
trusting_period = '14days'
trust_threshold = { numerator = '1', denominator = '3' }
"

function init_network() {
  local suffix=$1
  local genesis_path=$2

  mkdir -p ${DATA_DIR}/namada-${suffix}
  NAMADA_BASE_DIR=${DATA_DIR}/namada-${suffix}/.namada \
  ${NAMADAC} utils init-network \
    --unsafe-dont-encrypt \
    --genesis-path ${genesis_path} \
    --chain-prefix namada-test \
    --localhost \
    --dont-archive \
    --wasm-checksums-path ${CHECKSUM_PATH} \
  | awk '$1 == "Derived" {print $4}'
}

function copy_wasm() {
  local suffix=$1
  local chain_id=$2

  local base_dir=${DATA_DIR}/namada-${suffix}/.namada

  cp ${NAMADA_DIR}/wasm/checksums.json ${base_dir}/${chain_id}/setup/validator-0/.namada/${chain_id}/wasm/
  cp ${NAMADA_DIR}/wasm/*.wasm ${base_dir}/${chain_id}/setup/validator-0/.namada/${chain_id}/wasm/
  cp ${NAMADA_DIR}/wasm/checksums.json ${base_dir}/${chain_id}/wasm/
  cp ${NAMADA_DIR}/wasm/*.wasm ${base_dir}/${chain_id}/wasm/
}

function run_ledger() {
  local chain_id=$1
    > ${chain_id}.log 2>&1
}

function init_relayer_acc() {
  local suffix=$1
  local chain_id=$2
  local ledger_addr=$2

  local base_dir=${DATA_DIR}/namada-${suffix}/.namada
  local wasm_dir=${IBC_RS_DIR}/namada_wasm
  local wallet_dir=${IBC_RS_DIR}/namada_wallet/${chain_id}

  ${NAMADAW} --base-dir ${base_dir} \
    key gen --alias relayer --unsafe-dont-encrypt

  if [ "${suffix}" == "a" ]
  then
    mkdir -p ${wasm_dir}
    cp ${NAMADA_DIR}/wasm/checksums.json ${wasm_dir}
    cp ${NAMADA_DIR}/wasm/tx_ibc*.wasm ${wasm_dir}
  fi

  mkdir -p ${wallet_dir}
  cp ${base_dir}/${chain_id}/wallet.toml ${wallet_dir}
}

# ==== main ====

# for chain A
chain_id_a=$(init_network "a" ${GENESIS_PATH_A})

copy_wasm "a" ${chain_id_a}

${NAMADAN} --base-dir ${DATA_DIR}/namada-a/.namada/${chain_id_a}/setup/validator-0/.namada/ \
  --mode validator \
  ledger run > ${DATA_DIR}/namada-a/namada.log 2>&1 &
echo "Namada chain A's PID = $!"
sleep 5

init_relayer_acc "a" ${chain_id_a} ${LEDGER_ADDR_A}

# for chain B
sed "s/${NET_ADDR_A}/${NET_ADDR_B}/g" ${GENESIS_PATH_A} > ${GENESIS_PATH_B}
chain_id_b=$(init_network "b" ${GENESIS_PATH_B})

copy_wasm "b" ${chain_id_b}


${NAMADAN} --base-dir ${DATA_DIR}/namada-b/.namada/${chain_id_b}/setup/validator-0/.namada/ \
  --mode validator \
  ledger run > ${DATA_DIR}/namada-b/namada.log 2>&1 &
echo "Namada chain B's PID = $!"
sleep 5

init_relayer_acc "b" ${chain_id_b} ${LEDGER_ADDR_B}

# for the relayer
cd ${IBC_RS_DIR}
echo "${HERMES_CONFIG_TEMPLATE}" \
  | sed -e "s/_CHAIN_ID_A_/${chain_id_a}/g" -e "s/_CHAIN_ID_B_/${chain_id_b}/g" \
  > ${IBC_RS_DIR}/config_for_namada.toml

echo "2 Namada chains are running"
echo "You can use Hermes with ${IBC_RS_DIR}/config_for_namada.toml"
