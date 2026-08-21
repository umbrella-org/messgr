import { defineConfig } from "@playwright/test";

export default defineConfig({
  testDir: "./tests",
  fullyParallel: true,
  reporter: "list",
  use: {
    baseURL: process.env.API_BASE_URL || "http://localhost:8888",
    extraHTTPHeaders: {
      "Content-Type": "application/json",
    },
  },
});
