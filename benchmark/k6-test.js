import http from 'k6/http';
import { check, sleep } from 'k6';
import { Rate, Trend } from 'k6/metrics';

const errorRate = new Rate('errors');
const latency = new Trend('latency', true);

export const options = {
  scenarios: {
    // Warm-up
    warmup: {
      executor: 'constant-vus',
      vus: 20,
      duration: '10s',
      startTime: '0s',
      gracefulStop: '5s',
      tags: { phase: 'warmup' },
    },
    // Main steady load
    steady: {
      executor: 'constant-arrival-rate',
      rate: 4000,            // target RPS – adjust according to your machine
      timeUnit: '1s',
      duration: '45s',
      preAllocatedVUs: 150,
      maxVUs: 800,
      startTime: '10s',
      gracefulStop: '10s',
      tags: { phase: 'steady' },
    },
  },
  thresholds: {
    http_req_duration: ['p(50)<10', 'p(95)<50', 'p(99)<150'],
    http_req_failed: ['rate<0.02'],
    errors: ['rate<0.02'],
  },
};

const BASE = __ENV.TARGET || 'http://127.0.0.1:8080';

export default function () {
  const res = http.get(`${BASE}/`, {
    headers: {
      'User-Agent': 'k6-oxigate-bench/1.0',
      'Accept': '*/*',
    },
    timeout: '5s',
  });

  const ok = check(res, {
    'status is 200': (r) => r.status === 200,
  });

  errorRate.add(!ok);
  latency.add(res.timings.duration);
}

export function handleSummary(data) {
  const p50 = data.metrics.http_req_duration.values['p(50)'];
  const p95 = data.metrics.http_req_duration.values['p(95)'];
  const p99 = data.metrics.http_req_duration.values['p(99)'];
  const rps = data.metrics.http_reqs.values.rate;
  const failed = data.metrics.http_req_failed.values.rate;

  console.log('\n========== SUMMARY ==========');
  console.log(`Requests/s : ${rps.toFixed(1)}`);
  console.log(`p50        : ${p50.toFixed(2)} ms`);
  console.log(`p95        : ${p95.toFixed(2)} ms`);
  console.log(`p99        : ${p99.toFixed(2)} ms`);
  console.log(`Error rate : ${(failed * 100).toFixed(3)} %`);
  console.log('=============================\n');

  return {
    'stdout': textSummary(data, { indent: ' ', enableColors: true }),
  };
}

function textSummary(data, opts) {
  // minimal fallback
  return JSON.stringify({
    rps: data.metrics.http_reqs?.values?.rate,
    p50: data.metrics.http_req_duration?.values['p(50)'],
    p95: data.metrics.http_req_duration?.values['p(95)'],
    p99: data.metrics.http_req_duration?.values['p(99)'],
    error_rate: data.metrics.http_req_failed?.values?.rate,
  }, null, 2);
}
