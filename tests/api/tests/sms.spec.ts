import { test, expect } from "@playwright/test";

test.describe("POST /send/sms", () => {
  test("creates an sms message", async ({ request }) => {
    const res = await request.post("/send/sms", {
      data: {
        sender: "+15550001234",
        recipient: "+15559876543",
        body: "hello from playwright",
      },
    });

    expect(res.status()).toBe(201);

    const created = await res.json();
    expect(created).toHaveProperty("id");
    expect(created.sender).toBe("+15550001234");
    expect(created.recipient).toBe("+15559876543");
    expect(created.body).toBe("hello from playwright");
  });
});

test.describe("GET /send/sms", () => {
  test("lists sms messages including newly created one", async ({
    request,
  }) => {
    const body = "hello from playwright list check";

    const createRes = await request.post("/send/sms", {
      data: {
        sender: "+15550001234",
        recipient: "+15559876543",
        body,
      },
    });
    expect(createRes.status()).toBe(201);

    const listRes = await request.get("/send/sms");
    expect(listRes.status()).toBe(200);

    const messages = await listRes.json();
    expect(Array.isArray(messages)).toBe(true);
    expect(messages.some((m: { body: string }) => m.body === body)).toBe(
      true,
    );
  });
});
