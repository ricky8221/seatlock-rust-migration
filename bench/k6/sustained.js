// Sustained load: constant arrival rate (open model), so both services face the same offered
// load regardless of how fast they answer. This is the run latency is compared on.
//
//   BROWSE_RPS   read requests per second              (default 300)
//   RESERVE_RPS  reservation flows per second          (default 40; ~3.5 requests each)
//   DURATION     measured window                       (default 5m)
//
// Total offered load at defaults is roughly 300 + 40 * 3.5 = 440 req/s.
import { textSummary } from 'https://jslib.k6.io/k6-summary/0.1.0/index.js';
import { browse, createEvents, reserve, SUMMARY_TREND_STATS } from './lib.js';

const BROWSE_RPS = Number(__ENV.BROWSE_RPS || 300);
const RESERVE_RPS = Number(__ENV.RESERVE_RPS || 40);
const DURATION = __ENV.DURATION || '5m';

export const options = {
  setupTimeout: '5m',
  summaryTrendStats: SUMMARY_TREND_STATS,
  scenarios: {
    browse: {
      executor: 'constant-arrival-rate',
      exec: 'browseScenario',
      rate: BROWSE_RPS,
      timeUnit: '1s',
      duration: DURATION,
      preAllocatedVUs: 50,
      maxVUs: 400,
    },
    reserve: {
      executor: 'constant-arrival-rate',
      exec: 'reserveScenario',
      rate: RESERVE_RPS,
      timeUnit: '1s',
      duration: DURATION,
      preAllocatedVUs: 50,
      maxVUs: 400,
    },
  },
  thresholds: {
    // Not pass/fail gates for the comparison; they flag a run that should not be reported.
    biz_unexpected_status: ['count<1'],
    dropped_iterations: ['count<100'],
  },
};

// 10 events x 2,000 seats: large enough that random holds rarely collide at sustained rates.
export function setup() {
  const sections = [
    { name: 'FLOOR', rows: 20, seatsPerRow: 50, priceCents: 9500 },
    { name: 'BALCONY', rows: 20, seatsPerRow: 50, priceCents: 4500 },
  ];
  return { events: createEvents(10, sections, 'Sustained') };
}

export function browseScenario(data) {
  browse(data.events);
}

export function reserveScenario(data) {
  reserve(data.events);
}

export function handleSummary(data) {
  const out = __ENV.SUMMARY_OUT || '/results/sustained-summary.json';
  return {
    [out]: JSON.stringify(data, null, 2),
    stdout: textSummary(data, { indent: ' ', enableColors: false }),
  };
}
