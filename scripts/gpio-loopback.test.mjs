import assert from "node:assert/strict";
import test from "node:test";
import { boards, run } from "./gpio-loopback.mjs";

function fixture(board, settings = {}) {
  const { chip, output, input } = board;
  const calls = [];
  const reports = [];
  let level = "0";
  let boot = "c000000000000001";
  const reply = (payload) => ({ ok: true, encoding: "utf8", payload });
  const rpc = async (method, params) => {
    calls.push([method, params]);
    if (method === "link") return { listen_addr: "127.0.0.1:12345" };
    if (method === "workers") return { workers: [{
      worker_id: "chosen", online: true, boot_id_hex: boot,
      tools: ["gpio.info", "gpio.config", "gpio.read", "gpio.write"],
    }] };
    const { tool, args } = params;
    if (tool === "gpio.info") {
      return reply(args ? "pin=" + args + "\ncaps=in,out,pu,pd" :
        "chip=" + chip + "\nbackend=" + (settings.backend ?? "hardware"));
    }
    if (tool === "gpio.config") {
      return reply(args.startsWith(input) ? "pin=" + input + " mode=input pull=down" :
        "pin=" + output + " mode=output pull=none level=0 open_drain=0");
    }
    if (tool === "gpio.write") {
      level = args.split(" ")[1];
      if (level === "1" && settings.reboot) boot = "c000000000000002";
      return reply("pin=" + output + " level=" + level);
    }
    if (settings.timeout) throw new Error("tool_call timed out");
    return reply("pin=" + input + " level=" + (settings.wrongLevel ? "1" : level));
  };
  const execute = () => run({
    rpc, board, workerId: "chosen", broker: "127.0.0.1:1883", waitMs: 100,
    report: (entry) => reports.push(entry),
    startBridge: (address) => calls.push(["bridge", address]),
  });
  return { calls, reports, execute };
}

test("ESP32 selects MQTT and reads 0, 1, 0 independently", async () => {
  const { calls, reports, execute } = fixture(boards.esp32);
  await execute();
  assert.deepEqual(calls[0], ["link", { mode: "mqtt", mqtt_addr: "127.0.0.1:1883" }]);
  assert.deepEqual(reports.filter((entry) => entry.tool === "gpio.read").map((entry) => entry.actual.level), ["0", "1", "0"]);
});

test("STM32 starts a loopback listener and passes its bound address to the bridge", async () => {
  const { calls, execute } = fixture(boards.discovery);
  await execute();
  assert.deepEqual(calls.slice(0, 2), [
    ["link", { mode: "tcp", listen_addr: "127.0.0.1:0" }],
    ["bridge", "127.0.0.1:12345"],
  ]);
});

test("virtual backend is rejected before configuration", async () => {
  const { calls, execute } = fixture(boards["blue-pill"], { backend: "virtual" });
  await assert.rejects(execute(), /unexpected result/);
  assert.deepEqual(calls.filter(([method]) => method === "tool_call").map(([, params]) => params.tool), ["gpio.info"]);
});

for (const [name, settings] of [["wrong level", { wrongLevel: true }], ["timeout", { timeout: true }]]) {
  test(name + " fails the run and still drives the output low", async () => {
    const { calls, reports, execute } = fixture(boards.esp32, settings);
    await assert.rejects(execute());
    assert.ok(calls.some(([method, params]) => method === "tool_call" && params.tool === "gpio.write" && params.args === "IO25 0"));
    assert.deepEqual(reports.at(-1), { cleanup: "confirmed" });
  });
}

test("a replaced Worker fails the run and reports unconfirmed cleanup", async () => {
  const { calls, reports, execute } = fixture(boards.esp32, { reboot: true });
  await assert.rejects(execute(), /changed instances/);
  assert.equal(reports.at(-1).cleanup, "unconfirmed");
  assert.equal(calls.some(([method, params]) => method === "tool_call" && params.tool === "gpio.write" && params.args === "IO25 0"), false);
});
