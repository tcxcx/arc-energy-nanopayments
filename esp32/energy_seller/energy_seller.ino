/*
 * Energy Seller — the machine that gets paid for renewable energy telemetry.
 *
 * An ESP32 measures renewable generation and reports it to the Arc
 * nanopayments seller app. When an AI agent buys a reading via x402
 * ($0.001 USDC on Arc), the green LED blinks: the machine got paid.
 *
 * Pick ONE sensor mode below:
 *
 *  MODE_MOTOR — mini wind turbine. A small DC motor (toy/kit motor, ideally
 *    with a propeller) IS a generator: blow on it or spin it and it produces
 *    real voltage. Wiring: motor terminal A -> 10k resistor -> GPIO 34, and
 *    from GPIO 34 another 10k -> GND; motor terminal B -> GND. (The divider
 *    halves the voltage so spikes stay under 3.3V.)
 *
 *  MODE_LDR — "solar farm" with a photoresistor. Real irradiance mapped to
 *    panel output; a phone flashlight is the sun. Wiring: 3V3 -> LDR ->
 *    GPIO 34, and GPIO 34 -> 10k resistor -> GND.
 *
 *  MODE_INA219 — real solar panel via INA219 over I2C (SDA 21 / SCL 22).
 *
 *  MODE_SIM — no hardware at all; plausible solar curve.
 *
 * Paid LED -> GPIO 4 (with ~220R resistor) -> GND.
 *
 * Libraries (Arduino IDE Library Manager): ArduinoJson (+ Adafruit INA219
 * only for MODE_INA219).
 */

#include <WiFi.h>
#include <HTTPClient.h>
#include <ArduinoJson.h>

// ---------- CONFIG ----------
#define MODE_MOTOR   // <- pick one: MODE_MOTOR / MODE_LDR / MODE_INA219 / MODE_SIM

const char* WIFI_SSID     = "YOUR_WIFI";
const char* WIFI_PASSWORD = "YOUR_PASSWORD";

// Laptop running `npm run dev`, reachable on the same LAN
const char* SERVER_BASE   = "http://192.168.0.10:3000";
const char* MACHINE_KEY   = "workshop-secret";   // must match MACHINE_API_KEY
const char* DEVICE_ID     = "wind-esp32-01";

const int   ADC_PIN         = 34;    // MODE_MOTOR / MODE_LDR input
const int   LED_PAID_PIN    = 4;     // green LED: a sale happened
const int   LED_STATUS_PIN  = 2;     // builtin: WiFi/report heartbeat
const unsigned long REPORT_EVERY_MS = 5000;
const unsigned long POLL_EVERY_MS   = 3000;
// ----------------------------

#ifdef MODE_INA219
#include <Adafruit_INA219.h>
Adafruit_INA219 ina219;
#endif

unsigned long lastReport = 0;
unsigned long lastPoll = 0;
unsigned long lastSample = 0;
long lastPaidCount = -1;   // -1 = unknown until first poll
double whTotal = 0;        // accumulated energy since boot

void connectWifi() {
  WiFi.mode(WIFI_STA);
  WiFi.begin(WIFI_SSID, WIFI_PASSWORD);
  Serial.print("WiFi");
  while (WiFi.status() != WL_CONNECTED) {
    delay(400);
    Serial.print(".");
  }
  Serial.printf("\nConnected: %s\n", WiFi.localIP().toString().c_str());
}

// Read the generator. Returns watts; fills volts/amps/source/simulated.
float readPanel(float &volts, float &amps, const char* &source, bool &simulated) {
#if defined(MODE_INA219)
  source = "solar";
  simulated = false;
  volts = ina219.getBusVoltage_V();
  amps  = ina219.getCurrent_mA() / 1000.0f;
  if (amps < 0) amps = 0;
  return volts * amps;

#elif defined(MODE_MOTOR)
  // Real generation: the spinning motor's voltage, through a 1:2 divider.
  // Average a few samples — brushed motors are noisy.
  source = "wind";
  simulated = false;
  long acc = 0;
  for (int i = 0; i < 20; i++) { acc += analogRead(ADC_PIN); delay(2); }
  float adcVolts = (acc / 20.0f) * 3.3f / 4095.0f;
  volts = adcVolts * 2.0f;                  // undo divider
  // Estimated power into a virtual 10-ohm load: P = V^2 / R
  float watts = (volts * volts) / 10.0f;
  amps = volts > 0.01f ? watts / volts : 0;
  return watts;

#elif defined(MODE_LDR)
  // Real irradiance (LDR) mapped to the output of a virtual 5W panel.
  // Phone flashlight = the sun.
  source = "solar";
  simulated = false;
  long acc = 0;
  for (int i = 0; i < 10; i++) { acc += analogRead(ADC_PIN); delay(2); }
  float light = (acc / 10.0f) / 4095.0f;    // 0 dark .. 1 bright
  float watts = light * 5.0f;
  volts = 5.1f;
  amps = watts / volts;
  return watts;

#else // MODE_SIM
  source = "solar";
  simulated = true;
  float t = millis() / 1000.0f;
  float base = 3.0f + 1.5f * sinf(t / 60.0f);          // 1.5W..4.5W swing
  float noise = (random(-100, 100)) / 400.0f;          // +/- 0.25W
  float watts = max(0.2f, base + noise);
  volts = 5.1f;
  amps = watts / volts;
  return watts;
#endif
}

void reportReading(float volts, float amps, float watts, const char* source,
                   bool simulated) {
  HTTPClient http;
  http.begin(String(SERVER_BASE) + "/api/machine/reading");
  http.addHeader("Content-Type", "application/json");
  http.addHeader("x-machine-key", MACHINE_KEY);

  JsonDocument doc;
  doc["device_id"] = DEVICE_ID;
  doc["volts"] = volts;
  doc["amps"] = amps;
  doc["watts"] = watts;
  doc["wh_total"] = whTotal;
  doc["source"] = source;
  doc["simulated"] = simulated;

  String body;
  serializeJson(doc, body);
  int code = http.POST(body);
  Serial.printf("report %.2fW (%s) -> HTTP %d\n", watts, source, code);
  http.end();

  digitalWrite(LED_STATUS_PIN, HIGH);
  delay(30);
  digitalWrite(LED_STATUS_PIN, LOW);
}

void pollPayments() {
  HTTPClient http;
  http.begin(String(SERVER_BASE) + "/api/machine/status");
  int code = http.GET();
  if (code == 200) {
    JsonDocument doc;
    if (deserializeJson(doc, http.getString()) == DeserializationError::Ok) {
      long paid = doc["paid_count"] | 0;
      if (lastPaidCount >= 0 && paid > lastPaidCount) {
        long sales = paid - lastPaidCount;
        Serial.printf("PAID! +%ld sale(s), total %ld\n", sales, paid);
        for (long i = 0; i < sales; i++) {
          digitalWrite(LED_PAID_PIN, HIGH);
          delay(250);
          digitalWrite(LED_PAID_PIN, LOW);
          delay(150);
        }
      }
      lastPaidCount = paid;
    }
  }
  http.end();
}

void setup() {
  Serial.begin(115200);
  pinMode(LED_PAID_PIN, OUTPUT);
  pinMode(LED_STATUS_PIN, OUTPUT);

#if defined(MODE_MOTOR) || defined(MODE_LDR)
  analogReadResolution(12);
  pinMode(ADC_PIN, INPUT);
#endif

#ifdef MODE_INA219
  if (!ina219.begin()) {
    Serial.println("INA219 not found — check wiring");
    while (true) delay(1000);
  }
#endif

  connectWifi();
  lastSample = millis();
}

void loop() {
  if (WiFi.status() != WL_CONNECTED) connectWifi();

  unsigned long now = millis();

  // Integrate energy continuously (Wh = W * h)
  float volts, amps;
  const char* source;
  bool simulated;
  float watts = readPanel(volts, amps, source, simulated);
  whTotal += watts * ((now - lastSample) / 3600000.0);
  lastSample = now;

  if (now - lastReport >= REPORT_EVERY_MS) {
    lastReport = now;
    reportReading(volts, amps, watts, source, simulated);
  }

  if (now - lastPoll >= POLL_EVERY_MS) {
    lastPoll = now;
    pollPayments();
  }

  delay(100);
}
