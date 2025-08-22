# Testing Instructions

This directory provides an automated integration test for the MineShield proxy.

Run the test script:

```
./run.sh
```

The script builds the proxy, starts a lightweight Minecraft server that **requires Proxy Protocol v2**, launches the proxy with a test configuration, and opens 50 headless client connections through the proxy to validate concurrent forwarding.

Set `BOT_COUNT` to override the default number of clients.

Ensure `node` is installed. The script will install required npm packages automatically.
