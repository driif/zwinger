import assert from "node:assert/strict";
import { test } from "node:test";
import { echoHost } from "./echoHost.ts";

test("echo hosts are found", () => {
  for (const host of ["httpbin.org", "eu.httpbin.org", " HTTPBIN.org", "postman-echo.com", "echo.example.com", "echo.websocket.org"]) {
    assert.equal(echoHost(host), true, host);
  }
});

test("other hosts are not echo hosts", () => {
  for (const host of ["api.openai.com", "nothttpbin.org", "httpbin.org.evil.io", "postman-echo.com.io", "myecho.com", "api.echo.io"]) {
    assert.equal(echoHost(host), false, host);
  }
});
