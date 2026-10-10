// Peak load, two back-to-back scenarios. Peak RSS is the max over this whole script.
//
// 1. flash_sale: one small event (200 seats) and a burst of holds on it. Almost every request
//    loses the race (409), which exercises the optimistic-locking conflict path, the follow-up
//    read transaction, and the problem+json error body under contention.
//      FLASH_RPS     peak hold attempts per second   (default 1500)
//
// 2. saturation: the sustained mix (85% reads, 15% reservation flows), ramped until the service
//    stops keeping up. dropped_iterations and the latency curve show where each one saturates.
//      PEAK_RPS      final arrival rate              (default 3000)
import { textSummary } from 'https://jslib.k6.io/k6-summary/0.1.0/index.js';
import { browse, createEvents, reserve, SUMMARY_TREND_STATS } from './lib.js';

const FLASH_RPS = Number(__ENV.FLASH_RPS || 1500);
const PEAK_RPS = Number(__ENV.PEAK_RPS || 3000);

export const options = {
  setupTimeout: '5m',
  summaryTrendStats: SUMMARY_TREND_STATS,
  scenarios: {
    flash_sale: {
      executor: 'ramping-arrival-rate',
      exec: 'flashSale',
      startRate: 50,
      timeUnit: '1s',
      preAllocatedVUs: 200,
      maxVUs: 1000,
      stages: [
        { target: FLASH_RPS, duration: '20s' },
        { target: FLASH_RPS, duration: '30s' },
        { target: 0, duration: '5s' },
      ],
    },
    saturation: {
      executor: 'ramping-arrival-rate',
      exec: 'mixed',
      startTime: '65s', // after flash_sale, with a short gap
      startRate: 100,
      timeUnit: '1s',
      preAllocatedVUs: 200,
      maxVUs: 1500,
      stages: [
        { target: PEAK_RPS, duration: '2m' },
        { target: PEAK_RPS, duration: '30s' },
        { target: 0, duration: '10s' },
      ],
    },
  },
};

export function setup() {
  const flash = createEvents(
    1,
    [{ name: 'GA', rows: 10, seatsPerRow: 20, priceCents: 12000 }],
    'Flash sale',
  );
  const general = createEvents(
    10,
    [
      { name: 'FLOOR', rows: 20, seatsPerRow: 50, priceCents: 9500 },
      { name: 'BALCONY', rows: 20, seatsPerRow: 50, priceCents: 4500 },
    ],
    'Peak',
  );
  return { flash, general };
}

export function flashSale(data) {
  reserve(data.flash, 2);
}

export function mixed(data) {
  if (Math.random() < 0.85) browse(data.general);
  else reserve(data.general);
}

export function handleSummary(data) {
  const out = __ENV.SUMMARY_OUT || '/results/peak-summary.json';
  return {
    [out]: JSON.stringify(data, null, 2),
    stdout: textSummary(data, { indent: ' ', enableColors: false }),
  };
}
