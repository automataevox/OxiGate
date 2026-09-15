import http from 'k6/http';
import { check } from 'k6';
import { Rate, Trend } from 'k6/metrics';

const errorRate = new Rate('errors');
const latency = new Trend('latency', true);

const PROFILE = __ENV.PROFILE || 'steady';

const profiles = {
  steady: {
    steady: {
      executor: 'constant-arrival-rate',
      rate: 40000,
      timeUnit: '1s',
      duration: '60s',
      preAllocatedVUs: 200,
      maxVUs: 1000,
    },
  },
  lowTraffic: {
    steady: {
      executor: 'constant-arrival-rate',
      rate: 4000,
      timeUnit: '1s',
      duration: '45s',
      preAllocatedVUs: 3000,
      maxVUs: 6000,
    },
  },
  overload: {
    overload: {
      executor: 'constant-arrival-rate',
      rate: 75000,
      timeUnit: '1s',
      duration: '60s',
      preAllocatedVUs: 3000,
      maxVUs: 6000,
    },
  },
  stress: {
    stress: {
      executor: 'ramping-vus',
      startVUs: 50,
      stages: [
        { duration: '20s', target: 500 },
        { duration: '40s', target: 4000 },
        { duration: '15s', target: 0 },
      ],
    },
  },
  soak_4h: {
    soak_4h: {
      executor: 'constant-arrival-rate',
      rate: 40000,
      timeUnit: '1s',
      duration: '4h',
      preAllocatedVUs: 300,
      maxVUs: 600,   // hard cap — raise only if RPS can't be met
    },
  },
};

export const options = {
  summaryTrendStats: ['med', 'p(90)', 'p(95)', 'p(99)', 'p(99.9)', 'avg', 'min', 'max'],
  discardResponseBodies: true,

  scenarios: profiles[PROFILE] || profiles.steady,

  thresholds: {
    http_req_duration: ['p(50)<10', 'p(95)<50', 'p(99)<150'],
    http_req_failed: ['rate<0.01'],
    errors: ['rate<0.01'],
  },
};

const BASE = __ENV.TARGET || 'http://127.0.0.1:8080';

export default function () {
  const res = http.get(`${BASE}/`, {
    headers: {
      'User-Agent': 'k6-oxigate-bench/1.0',
      'Accept': '*/*',
    },
    timeout: '3s',
  });

  const ok = check(res, {
    'status is 200': (r) => r.status === 200,
  });

  errorRate.add(!ok);
  latency.add(res.timings.duration);
}

export function handleSummary(data) {
  const dur = data.metrics.http_req_duration?.values || {};

  const p50 = dur['med'] ?? dur['p(50)'] ?? null;
  const p95 = dur['p(95)'] ?? null;
  const p99 = dur['p(99)'] ?? null;
  const p999 = dur['p(99.9)'] ?? null;
  const avg = dur['avg'] ?? null;

  const rps = data.metrics.http_reqs?.values?.rate ?? 0;
  const failed = data.metrics.http_req_failed?.values?.rate ?? 0;

  const fmt = (val) => (val !== null && val !== undefined ? `${val.toFixed(3)} ms` : 'N/A');

  console.log(`\n========== OxiGate Benchmark [PROFILE: ${PROFILE}] ==========`);
  console.log(`Throughput : ${rps.toFixed(2)} req/s`);
  console.log(`Average    : ${fmt(avg)}`);
  console.log(`p50 (med)  : ${fmt(p50)}`);
  console.log(`p95        : ${fmt(p95)}`);
  console.log(`p99        : ${fmt(p99)}`);
  console.log(`p999       : ${fmt(p999)}`);
  console.log(`Error rate : ${(failed * 100).toFixed(4)} %`);
  console.log('===========================================================\n');

  return {
    'stdout': JSON.stringify({
      profile: PROFILE,
      rps: Number(rps.toFixed(2)),
      avg_ms: avg ? Number(avg.toFixed(3)) : null,
      p50_ms: p50 ? Number(p50.toFixed(3)) : null,
      p95_ms: p95 ? Number(p95.toFixed(3)) : null,
      p99_ms: p99 ? Number(p99.toFixed(3)) : null,
      p999_ms: p999 ? Number(p999.toFixed(3)) : null,
      error_rate_pct: Number((failed * 100).toFixed(4)),
    }, null, 2),
  };
}