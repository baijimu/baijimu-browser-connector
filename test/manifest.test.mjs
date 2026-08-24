import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const manifest = JSON.parse(await readFile(new URL("../connector.json", import.meta.url), "utf8"));
const cargo = await readFile(new URL("../Cargo.toml", import.meta.url), "utf8");
const packageJson = JSON.parse(await readFile(new URL("../package.json", import.meta.url), "utf8"));

test("release identity is aligned", () => {
  assert.equal(manifest.id, "com.baijimu.connector.browser");
  assert.equal(manifest.version, "0.1.0");
  assert.match(cargo, /version = "0\.1\.0"/);
  assert.equal(packageJson.version, manifest.version);
  assert.equal(manifest.source.repo, "baijimu/baijimu-browser-connector");
  assert.equal(manifest.source.revision, `v${manifest.version}`);
  assert.ok(manifest.icon.data.length > 1000);
});

test("runtime is host managed and loopback-only", () => {
  assert.equal(manifest.runtime.processOwnership, "host");
  assert.equal(manifest.runtime.healthCheck.url, "http://127.0.0.1:18121/healthz");
  assert.equal(manifest.transport.baseUrl, "http://127.0.0.1:18121");
  assert.ok(manifest.hostRequirements.capabilities.includes("connector.process.host-managed.v1"));
});

test("remote surface is typed and excludes raw execution", () => {
  const names = manifest.methods.map((method) => method.name);
  assert.equal(new Set(names).size, names.length);
  assert.deepEqual(names, [
    "status", "listProfiles", "createProfile", "updateProfile", "openSession",
    "listSessions", "closeSession", "getPage", "navigate", "snapshot", "click",
    "typeText", "pressKey", "waitFor", "uploadFiles", "screenshot",
  ]);
  for (const method of manifest.methods) {
    assert.equal(method.input_schema.type, "object");
    assert.equal(method.input_schema.additionalProperties, false);
  }
  assert.equal(names.some((name) => /eval|script|cdp|shell|command/i.test(name)), false);
  const navigate = manifest.methods.find((method) => method.name === "navigate");
  assert.equal(navigate.input_schema.properties.url.maxLength, 4096);
  const upload = manifest.methods.find((method) => method.name === "uploadFiles");
  assert.equal(upload.input_schema.properties.paths.maxItems, 10);
});

test("high-risk effects are represented by explicit capabilities", () => {
  const capabilities = new Map(manifest.remoteCapabilities.map((item) => [item.name, item.risk]));
  assert.equal(capabilities.get("browser.profile.manage"), "high");
  assert.equal(capabilities.get("browser.page.interact"), "high");
  assert.equal(capabilities.get("browser.file.upload"), "high");
});

