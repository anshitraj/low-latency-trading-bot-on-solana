#!/usr/bin/env bash
# Shared settings. Override via env. Every script prints what it will do and
# asks for confirmation before any billable or destructive action.
set -euo pipefail
: "${GCP_PROJECT:?set GCP_PROJECT}"
GCP_ZONE="${GCP_ZONE:-europe-west3-a}"
VM="${VM:-trader-vm}"
confirm() {
  [[ "${ASSUME_YES:-}" == "1" ]] && return 0
  read -r -p "$1 [type 'yes' to continue] " a
  [[ "$a" == "yes" ]] || { echo "aborted"; exit 1; }
}
gc() { gcloud --project "$GCP_PROJECT" "$@"; }
