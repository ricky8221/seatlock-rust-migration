// Shared helpers for the seatlock load scripts. Both services are driven by exactly the same
// requests; only BASE_URL differs.
import http from 'k6/http';
import { check } from 'k6';
import { Counter } from 'k6/metrics';

export const BASE = __ENV.BASE_URL || 'http://java:8080';
const JSON_HEADERS = { 'Content-Type': 'application/json' };

// Business outcomes, so a run can show the services did the same work, not just the same RPS.
export const holdsPlaced = new Counter('biz_holds_placed');
export const holdConflicts = new Counter('biz_hold_conflicts');
export const bookings = new Counter('biz_bookings_confirmed');
export const unexpected = new Counter('biz_unexpected_status');

// 409 is an expected business outcome under contention, not a transport failure.
http.setResponseCallback(http.expectedStatuses({ min: 200, max: 299 }, 409));

export const SUMMARY_TREND_STATS = ['avg', 'min', 'med', 'p(90)', 'p(95)', 'p(99)', 'max'];

export function uuid() {
  // RFC 4122 v4 from Math.random: good enough for idempotency keys in a load test.
  return 'xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx'.replace(/[xy]/g, (c) => {
    const r = (Math.random() * 16) | 0;
    return (c === 'x' ? r : (r & 0x3) | 0x8).toString(16);
  });
}

export function pick(arr) {
  return arr[Math.floor(Math.random() * arr.length)];
}

/** n distinct random elements of arr. */
export function sample(arr, n) {
  const chosen = new Set();
  while (chosen.size < Math.min(n, arr.length)) chosen.add(pick(arr));
  return [...chosen];
}

function headers(idempotent) {
  return idempotent ? { ...JSON_HEADERS, 'Idempotency-Key': uuid() } : JSON_HEADERS;
}

export function postJson(path, body, name, idempotent = false) {
  return http.post(`${BASE}${path}`, body === null ? null : JSON.stringify(body), {
    headers: headers(idempotent),
    tags: { name },
  });
}

export function get(path, name) {
  return http.get(`${BASE}${path}`, { tags: { name } });
}

export function del(path, name) {
  return http.del(`${BASE}${path}`, null, { tags: { name } });
}

export function expectStatus(res, allowed) {
  const ok = allowed.includes(res.status);
  if (!ok) {
    unexpected.add(1);
    if (__ENV.DEBUG) console.warn(`unexpected ${res.status} ${res.request.method} ${res.url}: ${res.body}`);
  }
  check(res, { [`status in ${allowed.join('/')}`]: () => ok });
  return ok;
}

/** startsAt 30 days out, so events never start during a run. */
function futureStart() {
  return new Date(Date.now() + 30 * 24 * 3600 * 1000).toISOString();
}

/**
 * Creates `count` events, each with the given sections, and returns their seat ids. Seat ids are
 * fetched once in setup() so the measured phase never pays for discovering them.
 */
export function createEvents(count, sections, label) {
  const events = [];
  for (let i = 0; i < count; i++) {
    const res = postJson(
      '/api/v1/events',
      { name: `${label} ${i}`, venue: 'Bench Hall', startsAt: futureStart(), sections },
      'setup_create_event',
    );
    if (res.status !== 201) throw new Error(`create event failed: ${res.status} ${res.body}`);
    const id = res.json('id');
    const seatIds = [];
    for (let page = 0; ; page++) {
      const p = get(`/api/v1/events/${id}/seats?size=500&page=${page}`, 'setup_list_seats');
      if (p.status !== 200) throw new Error(`list seats failed: ${p.status}`);
      p.json('items').forEach((s) => seatIds.push(s.id));
      if (page + 1 >= p.json('totalPages')) break;
    }
    events.push({ id, seatIds });
  }
  return events;
}

/** One read request, chosen like a browsing client would. */
export function browse(events) {
  const ev = pick(events);
  const r = Math.random();
  let res;
  if (r < 0.3) res = get(`/api/v1/events/${ev.id}/availability`, 'availability');
  else if (r < 0.6) {
    const page = Math.floor(Math.random() * Math.ceil(ev.seatIds.length / 50));
    res = get(`/api/v1/events/${ev.id}/seats?page=${page}&size=50`, 'list_seats');
  } else if (r < 0.75) res = get(`/api/v1/events/${ev.id}/seats?status=AVAILABLE&size=50`, 'list_seats_available');
  else if (r < 0.9) res = get(`/api/v1/events/${ev.id}`, 'get_event');
  else res = get('/api/v1/events?size=20', 'list_events');
  expectStatus(res, [200]);
}

/**
 * One reservation flow. Every path returns its seats to AVAILABLE (release, or confirm then
 * cancel) except the abandoned share, which the server's expiry job reclaims after the hold TTL.
 * That keeps inventory stable for the whole run and exercises the @Scheduled sweeper.
 *
 *   hold (1-4 seats, 50% with Idempotency-Key)
 *     409 -> conflict (expected under contention)
 *     201 -> 5% abandon | 40% release (DELETE) | 55% confirm -> get booking -> cancel
 */
export function reserve(events, maxSeats = 4) {
  const ev = pick(events);
  const seats = sample(ev.seatIds, 1 + Math.floor(Math.random() * maxSeats));
  const hold = postJson(
    `/api/v1/events/${ev.id}/holds`,
    { seatIds: seats, customerRef: `bench-${__VU}` },
    'place_hold',
    Math.random() < 0.5,
  );
  if (!expectStatus(hold, [201, 409])) return;
  if (hold.status === 409) {
    holdConflicts.add(1);
    return;
  }
  holdsPlaced.add(1);
  const holdId = hold.json('id');

  const r = Math.random();
  if (r < 0.05) return; // abandoned: the expiry job frees it
  if (r < 0.45) {
    expectStatus(del(`/api/v1/holds/${holdId}`, 'release_hold'), [200]);
    return;
  }
  const confirm = postJson(`/api/v1/holds/${holdId}/confirm`, null, 'confirm_hold', Math.random() < 0.5);
  if (!expectStatus(confirm, [201, 409])) return;
  if (confirm.status !== 201) return;
  bookings.add(1);
  const bookingId = confirm.json('id');
  expectStatus(get(`/api/v1/bookings/${bookingId}`, 'get_booking'), [200]);
  expectStatus(postJson(`/api/v1/bookings/${bookingId}/cancel`, null, 'cancel_booking'), [200]);
}
