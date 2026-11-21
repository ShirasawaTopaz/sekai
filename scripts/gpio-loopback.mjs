#!/usr/bin/env node
import { spawn } from "node:child_process";
import { createInterface } from "node:readline";
import { parseArgs } from "node:util";
import { setTimeout as sleep } from "node:timers/promises";
import { pathToFileURL } from "node:url";

export const boards = {
  "esp32": { chip: "esp32", output: "IO25", input: "IO26", mode: "mqtt" },
  "blue-pill": { chip: "stm32f103c8", output: "PB0", input: "PB1", mode: "tcp" },
  "discovery": { chip: "stm32f407vg", output: "PB0", input: "PB1", mode: "tcp" },
};

const tools = ["gpio.info", "gpio.config", "gpio.read", "gpio.write"];
const fields = (payload) => Object.fromEntries(payload.trim().split(/\s+/).map((part) => part.split("=")));

export async function run({ rpc, board, workerId, broker, startBridge, waitMs, report }) {
  if (board.mode === "mqtt") {
    await rpc("link", { mode: "mqtt", mqtt_addr: broker });
  } else {
    const { listen_addr } = await rpc("link", { mode: "tcp", listen_addr: "127.0.0.1:0" });
    startBridge(listen_addr);
  }

  async function worker() {
    const { workers } = await rpc("workers");
    return workers.find((entry) => entry.worker_id === workerId && entry.online);
  }

  const deadline = performance.now() + waitMs;
  let instance;
  do {
    instance = await worker();
    if (instance) break;
    await sleep(100);
  } while (performance.now() < deadline);
  if (!instance) throw new Error("Timed out waiting for Worker " + workerId);
  if (!/^[0-9a-f]{16}$/.test(instance.boot_id_hex) || /^0+$/.test(instance.boot_id_hex)
    || !tools.every((name) => instance.tools.includes(name))) {
    throw new Error("Worker Hello must contain a boot ID and all four GPIO tools");
  }
  report({ worker: instance, expected: { chip: board.chip, backend: "hardware" } });

  async function checkInstance() {
    if ((await worker())?.boot_id_hex !== instance.boot_id_hex) {
      throw new Error("Worker went offline or changed instances");
    }
  }

  async function call(tool, args, expected, matches) {
    let result;
    let actual;
    try {
      await checkInstance();
      result = await rpc("tool_call", { worker_id: workerId, tool, args });
      if (result.ok && result.encoding === "utf8" && !result.truncated) {
        actual = fields(result.payload);
      }
      if (!actual || !matches(actual)) {
        throw new Error(tool + " " + args + " failed or returned an unexpected result");
      }
      await checkInstance();
      return actual;
    } catch (error) {
      if (result === undefined) result = { error: error.message };
      throw error;
    } finally {
      report({ tool, args, expected, actual: actual ?? null, result });
    }
  }

  const exact = (tool, args, expected) => call(tool, args, expected,
    (actual) => Object.entries(expected).every(([key, value]) => actual[key] === value));
  const { chip, output, input } = board;
  await exact("gpio.info", "", { chip, backend: "hardware" });
  for (const [pin, caps] of [[output, ["out"]], [input, ["in", "pd"]]]) {
    await call("gpio.info", pin, { pin, caps }, (actual) =>
      actual.pin === pin && caps.every((cap) => actual.caps?.split(",").includes(cap)));
  }

  let outputAttempted = false;
  let failure;
  let cleanupFailure;
  const write = (level) => exact("gpio.write", output + " " + level, { pin: output, level });
  try {
    await exact("gpio.config", input + " in pull=down", { pin: input, mode: "input", pull: "down" });
    outputAttempted = true;
    await exact("gpio.config", output + " out level=0 pull=none open_drain=0", {
      pin: output, mode: "output", level: "0", pull: "none", open_drain: "0",
    });
    for (const [index, level] of ["0", "1", "0"].entries()) {
      if (index !== 0) await write(level);
      await exact("gpio.read", input, { pin: input, level });
    }
  } catch (error) {
    failure = error;
  } finally {
    if (outputAttempted) {
      try {
        await write("0");
        report({ cleanup: "confirmed" });
      } catch (error) {
        cleanupFailure = error;
        report({ cleanup: "unconfirmed", error: error.message, action: "Confirm the output level on the board." });
      }
    }
  }
  if (failure || cleanupFailure) throw failure ?? cleanupFailure;
}

function connectRouter(binary, timeoutMs) {
  const child = spawn(binary, ["--serve", "--allow-write", "--timeout-ms", String(timeoutMs)], {
    stdio: ["pipe", "pipe", "inherit"],
  });
  const pending = new Map();
  let nextId = 0;
  let stopped;
  function stop(error) {
    stopped = error;
    for (const { reject } of pending.values()) reject(error);
  }
  child.on("error", stop);
  child.stdin.on("error", stop);
  child.on("exit", (code, signal) => stop(new Error("Router exited (" + (signal ?? code) + ")")));
  createInterface({ input: child.stdout }).on("line", (line) => {
    try {
      const message = JSON.parse(line);
      const call = pending.get(message.id);
      if (!call) return;
      if (message.error) call.reject(new Error(JSON.stringify(message.error)));
      else call.resolve(message.result);
    } catch (error) {
      stop(error);
    }
  });
  return {
    child,
    rpc(method, params = {}) {
      if (stopped) return Promise.reject(stopped);
      const id = ++nextId;
      return new Promise((resolve, reject) => {
        const timer = setTimeout(() => reject(new Error(method + " timed out")), timeoutMs + 2_000);
        pending.set(id, { resolve, reject, timer });
        child.stdin.write(JSON.stringify({ id, method, params }) + "\n");
      }).finally(() => {
        clearTimeout(pending.get(id)?.timer);
        pending.delete(id);
      });
    },
  };
}

async function main() {
  const { values } = parseArgs({
    options: {
      board: { type: "string" },
      worker: { type: "string" },
      broker: { type: "string" },
      serial: { type: "string" },
      router: { type: "string", default: "sekai-router" },
      bridge: { type: "string", default: "stm32-bridge" },
      "wait-ms": { type: "string", default: "15000" },
      "timeout-ms": { type: "string", default: "3000" },
      help: { type: "boolean" },
    },
  });
  if (values.help) {
    console.log("Usage: node scripts/gpio-loopback.mjs --board esp32|blue-pill|discovery --worker ID");
    console.log("  esp32: --broker HOST:PORT    STM32: --serial PORT");
    console.log("  Options: --router PATH --bridge PATH --wait-ms N --timeout-ms N");
    return;
  }
  const board = Object.hasOwn(boards, values.board) ? boards[values.board] : null;
  const waitMs = Number(values["wait-ms"]);
  const timeoutMs = Number(values["timeout-ms"]);
  if (!board || !values.worker ||
    (board.mode === "mqtt" ? (!values.broker || values.serial) : (!values.serial || values.broker))) {
    throw new Error("Specify --board, --worker and the board's --broker or --serial. See --help.");
  }
  if (![waitMs, timeoutMs].every((n) => Number.isSafeInteger(n) && n > 0 && n <= 2_147_481_647)) {
    throw new Error("Timeouts must be positive integer milliseconds");
  }
  let bridge;
  let bridgeError;
  const { child, rpc } = connectRouter(values.router, timeoutMs);
  try {
    await run({
      rpc: (method, params) => bridgeError ? Promise.reject(bridgeError) : rpc(method, params),
      board,
      workerId: values.worker,
      broker: values.broker,
      waitMs,
      report: (entry) => console.log(JSON.stringify(entry)),
      startBridge: (address) => {
        bridge = spawn(values.bridge, [values.serial, address], { stdio: ["ignore", "ignore", "inherit"] });
        bridge.on("error", (error) => { bridgeError = error; });
        bridge.on("exit", (code, signal) => {
          bridgeError ??= new Error("Bridge exited (" + (signal ?? code) + ")");
        });
      },
    });
  } finally {
    child.stdin.end();
    bridge?.kill();
    child.kill();
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main().catch((error) => {
    console.error("gpio-loopback: " + error.message);
    process.exitCode = 1;
  });
}
