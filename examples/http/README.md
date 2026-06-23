# ESP32 reference firmware for DIY irrigation (HTTP path)

`localsky-irrigation.ino` turns an ESP32 relay board into a first-class LocalSky
controller using the `http_generic` (DIY HTTP) backend. LocalSky polls the board
and sends run/stop commands over a tiny REST contract; the board just switches
relays and reports state. No broker, no cloud.

## Flash it

1. Install the **ESP32 Arduino core** (Boards Manager > "esp32"). No external
   libraries are needed.
2. Open `localsky-irrigation.ino`, edit the top section:
   - `WIFI_SSID` / `WIFI_PASS`
   - the `zones[]` table: each row is `{id, name, relayGPIO}`. The `id` is what
     you'll enter as the zone's **controller station** in LocalSky.
   - optionally set `LOCALSKY_TOKEN` for bearer auth.
3. Flash (board: "ESP32 Dev Module"). The serial monitor prints the board's IP.

## Wire it into LocalSky

Setup wizard or Settings > Controllers > Add:

- **Kind:** DIY (HTTP)
- **Config:**
  ```json
  { "base_url": "http://<board-ip>", "bearer_token": null, "poll_interval_s": 10 }
  ```
- Click **Test connection** (hits `GET /status`), then **Scan zones** (imports
  `GET /zones`). Each zone's **controller station** is the board `id` (`"1"`..).

## The contract (for custom firmware)

| Method & path | Body | Response |
|---|---|---|
| `GET /status` | - | `{"firmware":"...","zones":[{"id":"1","running":true,"remaining_s":120}]}` |
| `GET /zones` | - | `{"zones":[{"id":"1","name":"Back Yard"}]}` |
| `POST /zone/{id}/run` | `{"seconds":600}` | `2xx` |
| `POST /zone/{id}/stop` | - | `2xx` |
| `POST /stop_all` | - | `2xx` |

Success is any `2xx`; return `401` for a bad token. Include `flow_gpm` in
`/status` only if a flow meter is wired (its presence tells LocalSky a meter
exists). Always enforce a board-side max-runtime so a lost network can't leave a
valve open, this sketch does that in `loop()`.

## Test the contract from your laptop

If you set `LOCALSKY_TOKEN`, add `-H "Authorization: Bearer $TOKEN"` to **every**
request below (an unauthenticated call returns `401`). Leave `AUTH` empty for an
open board.

```bash
BOARD=http://192.0.2.50
AUTH=""                         # or: AUTH='-H "Authorization: Bearer yourtoken"'
curl -s $AUTH $BOARD/status | jq .
curl -s $AUTH $BOARD/zones  | jq .
curl -s $AUTH -X POST $BOARD/zone/1/run -H 'content-type: application/json' -d '{"seconds":30}'
curl -s $AUTH $BOARD/status | jq '.zones[] | select(.id=="1")'   # running:true, remaining_s ~30
curl -s $AUTH -X POST $BOARD/zone/1/stop
curl -s $AUTH -X POST $BOARD/stop_all
```

Prefer MQTT (a broker, ESPHome, Tasmota)? Use the `examples/esphome/` reference
and the MQTT controller instead. See the DIY & ESP32 controllers doc for both.
