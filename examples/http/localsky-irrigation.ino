// LocalSky DIY irrigation controller, reference ESP32 firmware (HTTP path).
//
// A minimal, dependency-free Arduino sketch that implements LocalSky's
// `http_generic` controller contract. Flash it, point a "DIY (HTTP)" controller
// at this board's IP, and the setup wizard's Test connection + Scan zones work
// end to end. LocalSky owns all watering decisions and run durations; this board
// just switches relays and reports state.
//
// Contract implemented (see docs: Connecting Hardware > DIY & ESP32 controllers):
//   GET  /status          -> { firmware, zones:[{id,running,remaining_s}] }
//   GET  /zones           -> { zones:[{id,name}] }
//   POST /zone/{id}/run     body { "seconds": N }   (start zone for N seconds)
//   POST /zone/{id}/stop                            (stop zone)
//   POST /stop_all                                  (stop everything)
// Optional bearer auth: set LOCALSKY_TOKEN; LocalSky sends it as
//   `Authorization: Bearer <token>` when bearer_token is configured.
//
// Board-side safety: each zone has its own auto-off timer, so even if LocalSky
// or the network disappears mid-run the valve still closes. Never rely solely on
// the server to stop a valve.
//
// Requires: ESP32 Arduino core (board "ESP32 Dev Module"). No external libs.

#include <WiFi.h>
#include <WebServer.h>
#include <uri/UriBraces.h>

// ----- Edit these -----
static const char* WIFI_SSID = "your-wifi";
static const char* WIFI_PASS = "your-password";
// Leave empty for an open board on a trusted LAN; otherwise this must match the
// controller's bearer_token in LocalSky.
static const char* LOCALSKY_TOKEN = "";
static const char* FIRMWARE = "localsky-diy-1.0.0";

// Zones: stable id (what you put in each LocalSky zone's controller_station),
// a friendly name (shown in Scan zones), and the active-HIGH relay GPIO.
struct Zone {
  const char* id;
  const char* name;
  uint8_t pin;
  bool running;
  unsigned long offAtMs;  // millis() deadline; 0 = not running
};

Zone zones[] = {
  {"1", "Back Yard",  16, false, 0},
  {"2", "Front Yard", 17, false, 0},
  {"3", "Side Yard",  18, false, 0},
  {"4", "Shrubs",     19, false, 0},
};
static const size_t ZONE_COUNT = sizeof(zones) / sizeof(zones[0]);

// Hard cap on a single run (2h), mirroring LocalSky's own RUN_SECONDS_MAX.
static const unsigned long MAX_RUN_S = 7200;

WebServer server(80);

// ----- helpers -----
Zone* findZone(const String& id) {
  for (size_t i = 0; i < ZONE_COUNT; i++) {
    if (id == zones[i].id) return &zones[i];
  }
  return nullptr;
}

void setZone(Zone& z, bool on, unsigned long seconds = 0) {
  z.running = on;
  digitalWrite(z.pin, on ? HIGH : LOW);
  z.offAtMs = on ? (millis() + seconds * 1000UL) : 0;
}

// Minimal JSON string escaper for the hand-built bodies below. Escapes the two
// characters that would otherwise break a JSON string. Keeps the firmware
// dependency-free; zone ids/names are short labels, so this is sufficient.
String jsonEscape(const char* s) {
  String o;
  for (const char* p = s; *p; p++) {
    if (*p == '"' || *p == '\\') o += '\\';
    o += *p;
  }
  return o;
}

// Returns true if the request is authorized (token empty => always allowed).
bool authorized() {
  if (LOCALSKY_TOKEN[0] == '\0') return true;
  if (!server.hasHeader("Authorization")) return false;
  return server.header("Authorization") == String("Bearer ") + LOCALSKY_TOKEN;
}

bool guard() {
  if (authorized()) return true;
  server.send(401, "application/json", "{\"error\":\"unauthorized\"}");
  return false;
}

// Crude, dependency-free extraction of the integer after "seconds" in the body.
unsigned long parseSeconds(const String& body) {
  int k = body.indexOf("seconds");
  if (k < 0) return 0;
  int c = body.indexOf(':', k);
  if (c < 0) return 0;
  return strtoul(body.c_str() + c + 1, nullptr, 10);
}

// ----- handlers -----
void handleStatus() {
  if (!guard()) return;
  unsigned long now = millis();
  String out = "{\"firmware\":\"";
  out += jsonEscape(FIRMWARE);
  out += "\",\"zones\":[";
  for (size_t i = 0; i < ZONE_COUNT; i++) {
    Zone& z = zones[i];
    // Rollover-safe remaining: signed difference, not `offAtMs > now`, so it
    // stays correct across the ~49.7-day millis() wrap.
    long delta = (long)(z.offAtMs - now);
    unsigned long rem = (z.running && delta > 0) ? (unsigned long)delta / 1000UL : 0;
    if (i) out += ",";
    out += "{\"id\":\"";
    out += jsonEscape(z.id);
    out += "\",\"running\":";
    out += z.running ? "true" : "false";
    out += ",\"remaining_s\":";
    out += String(rem);
    out += "}";
  }
  out += "]}";
  server.send(200, "application/json", out);
}

void handleZones() {
  if (!guard()) return;
  String out = "{\"zones\":[";
  for (size_t i = 0; i < ZONE_COUNT; i++) {
    if (i) out += ",";
    out += "{\"id\":\"";
    out += jsonEscape(zones[i].id);
    out += "\",\"name\":\"";
    out += jsonEscape(zones[i].name);
    out += "\"}";
  }
  out += "]}";
  server.send(200, "application/json", out);
}

void handleRun() {
  if (!guard()) return;
  Zone* z = findZone(server.pathArg(0));
  if (!z) {
    server.send(404, "application/json", "{\"error\":\"unknown zone\"}");
    return;
  }
  unsigned long secs = parseSeconds(server.arg("plain"));
  if (secs == 0) secs = 600;            // sensible default if body omits seconds
  if (secs > MAX_RUN_S) secs = MAX_RUN_S;
  setZone(*z, true, secs);
  server.send(200, "application/json", "{\"ok\":true}");
}

void handleStop() {
  if (!guard()) return;
  Zone* z = findZone(server.pathArg(0));
  if (!z) {
    server.send(404, "application/json", "{\"error\":\"unknown zone\"}");
    return;
  }
  setZone(*z, false);
  server.send(200, "application/json", "{\"ok\":true}");
}

void handleStopAll() {
  if (!guard()) return;
  for (size_t i = 0; i < ZONE_COUNT; i++) setZone(zones[i], false);
  server.send(200, "application/json", "{\"ok\":true}");
}

void setup() {
  Serial.begin(115200);
  for (size_t i = 0; i < ZONE_COUNT; i++) {
    pinMode(zones[i].pin, OUTPUT);
    digitalWrite(zones[i].pin, LOW);
  }
  WiFi.mode(WIFI_STA);
  WiFi.begin(WIFI_SSID, WIFI_PASS);
  while (WiFi.status() != WL_CONNECTED) {
    delay(500);
    Serial.print(".");
  }
  Serial.println();
  Serial.print("LocalSky DIY board at http://");
  Serial.println(WiFi.localIP());

  // WebServer collects only headers it's told to; we need Authorization.
  const char* wanted[] = {"Authorization"};
  server.collectHeaders(wanted, 1);

  server.on("/status", HTTP_GET, handleStatus);
  server.on("/zones", HTTP_GET, handleZones);
  server.on(UriBraces("/zone/{}/run"), HTTP_POST, handleRun);
  server.on(UriBraces("/zone/{}/stop"), HTTP_POST, handleStop);
  server.on("/stop_all", HTTP_POST, handleStopAll);
  server.begin();
}

void loop() {
  server.handleClient();
  // Board-side auto-off watchdog: close any valve whose deadline has passed,
  // even if LocalSky never sends the stop (network/server outage).
  unsigned long now = millis();
  for (size_t i = 0; i < ZONE_COUNT; i++) {
    // Rollover-safe deadline check: (long)(now - deadline) >= 0 stays correct
    // across the ~49.7-day millis() wrap, where `now >= deadline` would not.
    if (zones[i].running && zones[i].offAtMs != 0 &&
        (long)(now - zones[i].offAtMs) >= 0) {
      setZone(zones[i], false);
    }
  }
}
