-- energy_readings: telemetry stream from the renewable-energy machine (ESP32 + solar panel)
create table public.energy_readings (
  id uuid primary key default gen_random_uuid(),
  created_at timestamptz not null default now(),
  device_id text not null,
  volts numeric not null,
  amps numeric not null,
  watts numeric not null,
  wh_total numeric not null default 0,
  source text not null default 'solar',
  simulated boolean not null default false,
  raw jsonb
);

create index energy_readings_device_created_idx
  on public.energy_readings (device_id, created_at desc);

alter table public.energy_readings enable row level security;

create policy "Allow public read access"
  on public.energy_readings for select
  using (true);

create policy "Allow service inserts"
  on public.energy_readings for insert
  to service_role
  with check (true);

alter publication supabase_realtime add table public.energy_readings;
