import http from "k6/http";
import { check } from "k6";

const BASE_URL = __ENV.BASE_URL || "http://localhost:8888";
const TARGET_RPS = Number(__ENV.TARGET_RPS || 200);

export const options = {
  scenarios: {
    create_sms: {
      executor: "constant-arrival-rate",
      exec: "createSms",
      rate: TARGET_RPS,
      timeUnit: "1s",
      duration: "30s",
      preAllocatedVUs: TARGET_RPS,
      maxVUs: TARGET_RPS * 4,
    },
    search_sms: {
      executor: "constant-arrival-rate",
      exec: "searchSms",
      rate: TARGET_RPS,
      timeUnit: "1s",
      duration: "30s",
      preAllocatedVUs: TARGET_RPS,
      maxVUs: TARGET_RPS * 4,
    },
  },
  thresholds: {
    http_req_duration: ["p(95)<300"],
    http_req_failed: ["rate<0.01"],
  },
};

function randomPhone() {
  const digits = String(Math.floor(Math.random() * 1e10)).padStart(10, "0");
  return `+1${digits}`;
}

export function createSms() {
  const res = http.post(
    `${BASE_URL}/send/sms`,
    JSON.stringify({
      sender: randomPhone(),
      recipient: randomPhone(),
      body: `load test message ${__VU}-${__ITER}`,
    }),
    { headers: { "Content-Type": "application/json" } },
  );

  check(res, { "create status is 201": (r) => r.status === 201 });
}

export function searchSms() {
  const res = http.get(`${BASE_URL}/sms/search?q=load`);

  check(res, { "search status is 200": (r) => r.status === 200 });
}
