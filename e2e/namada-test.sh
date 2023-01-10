#!/bin/bash

# This script executes ibc-rs' E2E test for Namada locally
# `make build-release` and `make build-wasm-scripts` on Namada directory in advance
# Run with `namada-test.sh ${namada_dir}`

set -xe

NAMADA_DIR=$1
if [ -z "${NAMADA_DIR}" ]
then
  echo "ERROR: Namada directory should be given"
  exit 1
fi
cd $(dirname $0)
IBC_RS_DIR=${PWD%/e2e*}

NAMADAC="${NAMADA_DIR}/target/release/namadac"
DATA_DIR="${IBC_RS_DIR}/data"
E2E_TEST_LOG="ibc_e2e.log"

LEDGER_ADDR_A="127.0.0.1:27657"
LEDGER_ADDR_B="127.0.0.1:28657"
INITIAL_BALANCE=10000

function init_relayer_balance() {
  local suffix=$1
  local ledger_addr=$2

  local base_dir=${DATA_DIR}/namada-${suffix}/.namada

  local cnt=0
  local cnt_max=$((${INITIAL_BALANCE} / 1000))
  while [ ${cnt} -lt ${cnt_max} ]
  do
      ${NAMADAC} --base-dir ${base_dir} \
        transfer \
        --source faucet \
        --target relayer \
        --token nam \
        --amount 1000 \
        --signer relayer \
        --ledger-address ${ledger_addr}

      cnt=$((${cnt} + 1))
  done
}

# ==== main ====

# Run 2 Namada chains
${IBC_RS_DIR}/scripts/setup-namada.sh ${NAMADA_DIR}

# Initialize the balances
init_relayer_balance "a" ${LEDGER_ADDR_A}
init_relayer_balance "b" ${LEDGER_ADDR_B}

cd ${IBC_RS_DIR}
python3 e2e/run.py --config ${IBC_RS_DIR}/config_for_namada.toml 2>&1 | tee e2e/${E2E_TEST_LOG}

killall namadan
