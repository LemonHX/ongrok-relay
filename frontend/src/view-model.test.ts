import { describe, expect, it } from "vitest";
import { endpointLabel, metricPoints, type MetricSample } from "./view-model";

const sample = (cpu_percent: number | null): MetricSample => ({
  recorded_at_unix_ms: 1,
  rtt_ms: null,
  snapshot: { cpu_percent, memory_percent: null, load_average: null },
});

describe("metricPoints", () => {
  it("skips null samples and clamps values to the chart bounds", () => {
    expect(metricPoints([sample(null), sample(-5), sample(120)], (item) => item.snapshot.cpu_percent)).toEqual([
      "50,100",
      "100,0",
    ]);
  });

  it("keeps only the most recent 60 samples", () => {
    const metrics = Array.from({ length: 61 }, (_, index) => sample(index));
    const points = metricPoints(metrics, (item) => item.snapshot.cpu_percent);
    expect(points).toHaveLength(60);
    expect(points[0]).toBe("0,99");
  });
});

describe("endpointLabel", () => {
  it("formats host and port without creating malformed placeholders", () => {
    expect(endpointLabel(null, null)).toBe("--");
    expect(endpointLabel("ssh.example", null)).toBe("ssh.example");
    expect(endpointLabel("ssh.example", 22022)).toBe("ssh.example:22022");
  });
});
