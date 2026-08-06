#!/bin/sh
# Chat indexer (vendored kasia fork). Reads config from env (KASPA_NODE_WBORSH_URL,
# NETWORK_TYPE, KASIA_INDEXER_DB_ROOT, KASIA_API_BIND). Own embedded fjall store.
exec /app/kachat-chat-indexer
