import assert from "node:assert/strict";
import { test } from "node:test";
import { capacityText, resourceWarning, usageText } from "./hostCapacity.ts";

const mac = { cpus: 8, memoryMib: 16384 };

test("values within the Mac give no warning", () => {
  assert.equal(resourceWarning(8, 14336, mac), undefined);
  assert.equal(resourceWarning(1, 512, mac), undefined);
});

test("more CPUs than the Mac has give a warning", () => {
  assert.match(resourceWarning(9, 512, mac) ?? "", /more CPUs than this Mac has \(8\)/);
});

test("memory above the Mac memory minus the reserve gives a warning", () => {
  assert.match(resourceWarning(1, 14337, mac) ?? "", /more memory than this Mac can give a VM \(14336 MiB\)/);
});

test("both problems show in one warning", () => {
  const warning = resourceWarning(9, 16384, { cpus: 4, memoryMib: 8192 }) ?? "";
  assert.match(warning, /CPUs .* and more memory/);
});

test("the capacity shows the memory in GiB", () => {
  assert.equal(capacityText(mac), "This Mac: 8 CPUs, 16 GiB");
});

test("the use of a VM shows the CPUs in use and the memory in MiB", () => {
  const usage = { cpuPercent: 137.4, memoryBytes: 300 * 1024 * 1024, memoryLimitBytes: 512 * 1024 * 1024 };
  assert.equal(usageText(usage, 2), "uses 1.4 of 2 CPUs and 300 of 512 MiB");
});
