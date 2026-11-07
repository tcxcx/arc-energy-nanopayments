import { NextResponse } from "next/server";

export async function GET() {
  const baseUrl = (process.env.AUTARQIA_NODE_URL ?? "http://127.0.0.1:8787").replace(
    /\/$/,
    "",
  );
  try {
    const [providerRes, healthRes] = await Promise.all([
      fetch(`${baseUrl}/.well-known/autarqia/provider.json`, {
        cache: "no-store",
      }),
      fetch(`${baseUrl}/healthz`, { cache: "no-store" }),
    ]);
    if (!providerRes.ok || !healthRes.ok) {
      return NextResponse.json(
        {
          online: false,
          node_url: baseUrl,
          status: providerRes.status,
        },
        { status: 503 },
      );
    }
    const [provider, health] = await Promise.all([
      providerRes.json(),
      healthRes.json(),
    ]);
    return NextResponse.json({
      online: true,
      node_url: baseUrl,
      provider,
      health,
    });
  } catch (err) {
    return NextResponse.json(
      {
        online: false,
        node_url: baseUrl,
        error: err instanceof Error ? err.message : "autarqia-node unreachable",
      },
      { status: 503 },
    );
  }
}
