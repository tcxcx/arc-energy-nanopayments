import { createClient } from "@supabase/supabase-js";
import { NextRequest, NextResponse } from "next/server";

const supabase = createClient(
  process.env.NEXT_PUBLIC_SUPABASE_URL!,
  process.env.SUPABASE_SERVICE_ROLE_KEY!,
);

/**
 * Ingestion endpoint for the energy machine (ESP32).
 * The device POSTs a reading every few seconds; no payment involved here —
 * this is the machine reporting its own production. Selling happens at
 * /api/premium/energy.
 */
export async function POST(req: NextRequest) {
  const machineKey = process.env.MACHINE_API_KEY;
  if (machineKey && req.headers.get("x-machine-key") !== machineKey) {
    return NextResponse.json({ error: "Unauthorized" }, { status: 401 });
  }

  let body: Record<string, unknown>;
  try {
    body = await req.json();
  } catch {
    return NextResponse.json({ error: "Invalid JSON" }, { status: 400 });
  }

  const deviceId = typeof body.device_id === "string" ? body.device_id : null;
  const volts = Number(body.volts);
  const amps = Number(body.amps);
  const watts = Number(body.watts);
  const whTotal = Number(body.wh_total ?? 0);

  if (!deviceId || [volts, amps, watts, whTotal].some(Number.isNaN)) {
    return NextResponse.json(
      { error: "Expected device_id, volts, amps, watts, wh_total" },
      { status: 400 },
    );
  }

  const { error } = await supabase.from("energy_readings").insert({
    device_id: deviceId,
    volts,
    amps,
    watts,
    wh_total: whTotal,
    source: typeof body.source === "string" ? body.source : "solar",
    simulated: body.simulated === true,
    raw: body,
  });

  if (error) {
    console.error("Failed to record energy reading:", error.message);
    return NextResponse.json({ error: error.message }, { status: 500 });
  }

  return NextResponse.json({ ok: true });
}
