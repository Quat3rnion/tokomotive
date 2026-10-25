# Drain aborted streams to get exact usage

The Provider bills a streamed request even when the client disconnects. The Proxy continues to read the upstream stream to its end after a client disconnect, so that it gets the usage that the Provider reports. We accept the extra upstream output that the client does not receive, because an estimated Ledger Entry would make Cost and Profit wrong. If the upstream fails before it reports usage, the Proxy records the known input tokens and marks the Ledger Entry as an Estimated Entry.
