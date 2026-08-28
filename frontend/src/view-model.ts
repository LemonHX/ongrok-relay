export type MetricSample = {
  recorded_at_unix_ms: number;
  rtt_ms: number | null;
  snapshot: {
    cpu_percent: number | null;
    memory_percent: number | null;
    load_average: number | null;
  };
};

/** Convert sparse metric samples into a bounded SVG polyline coordinate list. */
export function metricPoints(metrics: MetricSample[], read: (metric: MetricSample) => number | null): string[] {
  const values = metrics.slice(-60);
  return values.flatMap((metric, index) => {
    const value = read(metric);
    if (value == null || !Number.isFinite(value)) return [];
    const x = values.length < 2 ? 0 : (index / (values.length - 1)) * 100;
    const y = 100 - Math.min(100, Math.max(0, value));
    return [`${x},${y}`];
  });
}

export function endpointLabel(host: string | null, port: number | null): string {
  if (!host) return "--";
  return port == null ? host : `${host}:${port}`;
}
