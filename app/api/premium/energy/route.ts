import { createClient } from "@supabase/supabase-js";
import { NextRequest, NextResponse } from "next/server";
import { withGateway } from "@/lib/x402";

const supabase = createClient(
  process.env.NEXT_PUBLIC_SUPABASE_URL!,
  process.env.SUPABASE_SERVICE_ROLE_KEY!,
);

/**
 * The machine's product: verified renewable-energy production telemetry.
 * Each $0.001 purchase returns the latest reading from the solar-powered
 * ESP32 plus production stats over the last 15 minutes.
 */
const handler = async (_req: NextRequest) => {
  const since = new Date(Date.now() - 15 * 60 * 1000).toISOString();

  const { data: readings } = await supabase
    .from("energy_readings")
    .select("device_id, created_at, volts, amps, watts, wh_total, source, simulated")
    .gte("created_at", since)
    .order("created_at", { ascending: false })
    .limit(200);

  if (!readings || readings.length === 0) {
    // No live device connected — serve a simulated solar reading so the
    // endpoint (and the demo) never breaks.
    const watts = 3.2 + Math.random() * 0.6;
    return NextResponse.json({
      device_id: "solar-esp32-sim",
      source: "solar",
      simulated: true,
      reading: {
        timestamp: new Date().toISOString(),
        volts: 5.1,
        amps: Number((watts / 5.1).toFixed(3)),
        watts: Number(watts.toFixed(2)),
        wh_total: 0,
      },
      window_15m: { samples: 0, avg_watts: null, wh_produced: null },
    });
  }

  const latest = readings[0];
  const oldest = readings[readings.length - 1];
  const avgWatts =
    readings.reduce((sum, r) => sum + Number(r.watts), 0) / readings.length;

  return NextResponse.json({
    device_id: latest.device_id,
    source: latest.source,
    simulated: latest.simulated,
    reading: {
      timestamp: latest.created_at,
      volts: Number(latest.volts),
      amps: Number(latest.amps),
      watts: Number(latest.watts),
      wh_total: Number(latest.wh_total),
    },
    window_15m: {
      samples: readings.length,
      avg_watts: Number(avgWatts.toFixed(2)),
      wh_produced: Number(
        (Number(latest.wh_total) - Number(oldest.wh_total)).toFixed(3),
      ),
    },
  });
};

export const GET = withGateway(handler, "$0.001", "/api/premium/energy");
