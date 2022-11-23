#!/bin/bash

# This script executes ibc-rs' E2E test for Namada locally
# `make build` and `make build-wasm-scripts` on Namada directory in advance,
# Run with `namada-test.sh ${namada_dir}`

set -e

NAMADA_DIR=$1
if [ -z "${NAMADA_DIR}" ]
then
  echo "ERROR: Namada directory should be given"
  exit 1
fi
cd $(dirname $0)
IBC_RS_DIR=${PWD%/e2e*}

NAMADAC="${NAMADA_DIR}/target/debug/namadac"
NAMADAN="${NAMADA_DIR}/target/debug/namadan"
NAMADAW="${NAMADA_DIR}/target/debug/namadaw"
GENESIS_PATH_A="${NAMADA_DIR}/genesis/e2e-tests-single-node.toml"
GENESIS_PATH_B="${NAMADA_DIR}/genesis/e2e-tests-single-node-b.toml"
CHECKSUM_PATH="${NAMADA_DIR}/wasm/checksums.json"
CHAIN_LOG_A="ibc_e2e_chain_a.log"
CHAIN_LOG_B="ibc_e2e_chain_b.log"
E2E_TEST_LOG="ibc_e2e.log"

NET_ADDR_A="127.0.0.1:27656"
NET_ADDR_B="127.0.0.1:28656"
LEDGER_ADDR_A="127.0.0.1:27657"
LEDGER_ADDR_B="127.0.0.1:28657"
INITIAL_BALANCE=2000

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
  local genesis_path=$1
  ANOMA_BASE_DIR=${IBC_RS_DIR}/.anoma \
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
  local chain_id=$1

  cp ${NAMADA_DIR}/wasm/checksums.json ${IBC_RS_DIR}/.anoma/${chain_id}/setup/validator-0/.anoma/${chain_id}/wasm/
  cp ${NAMADA_DIR}/wasm/*.wasm ${IBC_RS_DIR}/.anoma/${chain_id}/setup/validator-0/.anoma/${chain_id}/wasm/
  cp ${NAMADA_DIR}/wasm/checksums.json ${IBC_RS_DIR}/.anoma/${chain_id}/wasm/
  cp ${NAMADA_DIR}/wasm/*.wasm ${IBC_RS_DIR}/.anoma/${chain_id}/wasm/
}

function run_ledger() {
  local chain_id=$1
    > ${chain_id}.log 2>&1
}

function init_relayer_acc() {
  local chain_id=$1
  local ledger_addr=$2

  local wasm_dir=${IBC_RS_DIR}/namada_wasm
  local wallet_dir=${IBC_RS_DIR}/namada_wallet/${chain_id}

  ${NAMADAW} --base-dir ${IBC_RS_DIR}/.anoma \
    key gen --alias relayer --unsafe-dont-encrypt

  local cnt=0
  local cnt_max=$((${INITIAL_BALANCE} / 1000))
  while [ ${cnt} -lt ${cnt_max} ]
  do
      ${NAMADAC} --base-dir ${IBC_RS_DIR}/.anoma \
        transfer \
        --source faucet \
        --target relayer \
        --token nam \
        --amount 1000 \
        --signer relayer \
        --ledger-address ${ledger_addr}

      cnt=$((${cnt} + 1))
  done

  mkdir -p ${wasm_dir}
  cp ${NAMADA_DIR}/wasm/checksums.json ${wasm_dir}
  cp ${NAMADA_DIR}/wasm/tx_ibc*.wasm ${wasm_dir}

  mkdir -p ${wallet_dir}
  cp ${IBC_RS_DIR}/.anoma/${chain_id}/wallet.toml ${wallet_dir}
}

# ==== main ====

# for chain A
chain_id_a=$(init_network ${GENESIS_PATH_A})

copy_wasm ${chain_id_a}

${NAMADAN} --base-dir ${IBC_RS_DIR}/.anoma/${chain_id_a}/setup/validator-0/.anoma/ \
  --mode validator \
  ledger run > ${CHAIN_LOG_A} 2>&1 &
pid_a=$(echo $!)
sleep 5

init_relayer_acc ${chain_id_a} ${LEDGER_ADDR_A}

# for chain B
sed "s/${NET_ADDR_A}/${NET_ADDR_B}/g" ${GENESIS_PATH_A} > ${GENESIS_PATH_B}
chain_id_b=$(init_network ${GENESIS_PATH_B})

copy_wasm ${chain_id_b}


${NAMADAN} --base-dir ${IBC_RS_DIR}/.anoma/${chain_id_b}/setup/validator-0/.anoma/ \
  --mode validator \
  ledger run > ${CHAIN_LOG_B} 2>&1 &
pid_b=$(echo $!)
sleep 5

init_relayer_acc ${chain_id_b} ${LEDGER_ADDR_B}

# for the relayer
cd ${IBC_RS_DIR}
echo "${HERMES_CONFIG_TEMPLATE}" \
  | sed -e "s/_CHAIN_ID_A_/${chain_id_a}/g" -e "s/_CHAIN_ID_B_/${chain_id_b}/g" \
  > namada_e2e_config.toml

python3 e2e/run.py --config namada_e2e_config.toml 2>&1 | tee e2e/${E2E_TEST_LOG}

kill ${pid_a} ${pid_b}
