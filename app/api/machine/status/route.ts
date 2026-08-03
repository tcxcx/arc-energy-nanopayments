import { createClient } from "@supabase/supabase-js";
import { NextResponse } from "next/server";

const supabase = createClient(
  process.env.NEXT_PUBLIC_SUPABASE_URL!,
  process.env.SUPABASE_SERVICE_ROLE_KEY!,
);

/**
 * Polled by the ESP32 every few seconds. Returns the total number of settled
 * payments for the energy endpoint; the device remembers the last count and
 * blinks its LED once per new sale ("the machine got paid").
 */
export async function GET() {
  const { count, error } = await supabase
    .from("payment_events")
    .select("*", { count: "exact", head: true })
    .eq("endpoint", "/api/premium/energy");

  if (error) {
    return NextResponse.json({ error: error.message }, { status: 500 });
  }

  return NextResponse.json({ paid_count: count ?? 0 });
}
