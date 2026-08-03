# Demo: La máquina solar que factura — Nanopayments & Machine Economy en Arc

**Taller Dev3Pack x Arc — 7 de agosto, Buenos Aires.**

Un ESP32 mide la producción de un panel solar (INA219) y **vende sus lecturas de
telemetría** a un agente de IA por USD 0.001 en USDC sobre Arc testnet, vía
x402 + Circle Gateway (gasless, batcheado). Cuando entra un pago, el LED verde
parpadea: **la máquina cobró**.

```
[panel solar] -> [ESP32 + INA219] --POST /api/machine/reading--> [Next.js seller]
                       ^                                              |
                       |  GET /api/machine/status (paid_count)        | x402 402/settle
                       |  LED verde si hay venta nueva                v
                  [agente LangChain] --GET /api/premium/energy--> paga $0.001 USDC
```

## Qué se agregó sobre el repo original

| Archivo | Rol |
| --- | --- |
| `supabase/migrations/20260802000000_energy_readings.sql` | Tabla `energy_readings` + realtime |
| `app/api/machine/reading/route.ts` | Ingesta de telemetría del ESP32 (header `x-machine-key`) |
| `app/api/machine/status/route.ts` | Polling del ESP32: cantidad de ventas pagadas |
| `app/api/premium/energy/route.ts` | **El producto**: lectura + stats 15 min, x402 $0.001 |
| `esp32/energy_seller/energy_seller.ino` | Sketch: INA219 o modo simulado + LED de cobro |
| `agent.mts` | El agente ahora conoce `/api/premium/energy` |

## Setup (antes del jueves)

1. `npm install`
2. `cp .env.example .env.local` y completar (incluye `MACHINE_API_KEY`)
3. `npm run generate-wallets` → fondear el buyer en https://faucet.circle.com (Arc Testnet)
4. Supabase: `npx supabase start && npx supabase migration up` (o `db push` remoto)
5. `npm run dev`
6. **ESP32**: abrir `esp32/energy_seller/energy_seller.ino` en Arduino IDE
   - Completar `WIFI_SSID` / `WIFI_PASSWORD` / `SERVER_BASE` (IP de tu laptop en la LAN) / `MACHINE_KEY`
   - Elegir el modo con un `#define` (sin panel solar hay 3 opciones):
     - **`MODE_MOTOR` (recomendado)** — mini turbina eólica: motor DC chico (de juguete/kit, ideal con hélice) + 2 resistencias de 10k como divisor a GPIO 34. Soplás la hélice → generás voltaje REAL → la máquina vende tu energía. Demo tactil, pasás el "molino" por el público.
     - **`MODE_LDR`** — granja solar con fotorresistencia (viene en todo kit Arduino): 3V3 → LDR → GPIO 34 → 10k → GND. La linterna del celu es "el sol": la acercás y la producción sube en vivo.
     - **`MODE_SIM`** — sin nada conectado, curva solar simulada.
     - **`MODE_INA219`** — panel solar real por I2C (si conseguís uno).
   - Librerías: ArduinoJson (+ Adafruit INA219 solo para ese modo)
7. Probar venta: `npm run agent -- "Compra una lectura de energía solar en http://localhost:3000/api/premium/energy" --limit 0.5`

## Guion de los 20 minutos (bloque demo)

1. Proyector: dashboard (`/dashboard`) a la izquierda, terminal del agente a la derecha, ESP32 visible a cámara.
2. Mostrar el ESP32 reportando (LED azul parpadea cada 5s = telemetría).
3. Correr el agente: pide la lectura → recibe **HTTP 402** → firma autorización offchain → reintenta → 200 con los datos de producción.
4. **LED verde parpadea** = la máquina cobró. Dashboard suma $0.001.
5. Loop: `--limit 0.10` y dejarlo comprar lecturas en serie → el dashboard acumula ventas sub-centavo en vivo.
6. Cerrar con la tx de settlement batcheado en https://testnet.arcscan.app — muchos nanopagos, un settlement onchain.

**Puntos a narrar**: USDC es el gas (fees estables), finality sub-segundo, Gateway batchea → sub-centavo viable, el buyer es gasless. ERC-8004/8183 (identidad + job settlement del agente/máquina) van en slides como la capa siguiente.

## Fallbacks

- **Sin WiFi confiable**: hotspot del teléfono; ESP32 y laptop en la misma red.
- **Sin ESP32 / sin sensor**: `/api/premium/energy` responde una lectura simulada si no hay telemetría — la demo de pagos funciona igual.
- **Sin nada**: grabar un video del flujo completo antes del jueves.
