import { describe, expect, it, vi } from "vitest";
import { portOfEndpoint, startSidecar, stopSidecar } from "../sidecar";

describe("portOfEndpoint", () => {
  it("常见写法都取得出端口", () => {
    expect(portOfEndpoint("http://127.0.0.1:8787")).toBe(8787);
    expect(portOfEndpoint("http://127.0.0.1:8787/")).toBe(8787);
    expect(portOfEndpoint("http://localhost:9000/systemOne")).toBe(9000);
    expect(portOfEndpoint("127.0.0.1:8787")).toBe(8787);
    expect(portOfEndpoint("http://[::1]:8787")).toBe(8787);
  });

  it("取不出就说取不出，不替用户编一个端口", () => {
    expect(portOfEndpoint("")).toBeNull();
    expect(portOfEndpoint("http://127.0.0.1")).toBeNull();
    expect(portOfEndpoint("http://[::1]")).toBeNull(); // 冒号在方括号里，不是端口分隔符
    expect(portOfEndpoint("http://127.0.0.1:99999")).toBeNull();
    expect(portOfEndpoint("http://127.0.0.1:abc")).toBeNull();
  });
});

describe("startSidecar / stopSidecar", () => {
  it("端口照着配置里的服务商给，子目录带上", async () => {
    const invokeImpl = vi.fn(async () => 4242);
    const pid = await startSidecar({
      dir: "C:\\repo\\scripts\\laya-sidecar",
      endpoint: "http://127.0.0.1:9001",
      subfolder: "multilingual",
      invokeImpl,
    });
    expect(pid).toBe(4242);
    expect(invokeImpl).toHaveBeenCalledWith("decision_sidecar_start", {
      dir: "C:\\repo\\scripts\\laya-sidecar",
      port: 9001,
      subfolder: "multilingual",
    });
  });

  it("服务商里没写端口时干脆不传 port，让原生用它那个默认值", async () => {
    const invokeImpl = vi.fn(async (_command: string, _args: Record<string, unknown>) => 1);
    await startSidecar({ dir: "/tmp/s", endpoint: "http://127.0.0.1", invokeImpl });
    const args = invokeImpl.mock.calls[0][1];
    expect("port" in args).toBe(false);
    expect("subfolder" in args).toBe(false);
  });

  it("原生报回的不是进程号就抛错，不要把 NaN 当成 pid 显示出去", async () => {
    await expect(
      startSidecar({ dir: "/tmp/s", endpoint: "http://127.0.0.1:8787", invokeImpl: async () => null }),
    ).rejects.toThrow(/进程号/);
  });

  it("stop 把原生的布尔如实传出去（false 是有意义的答案，不是失败）", async () => {
    expect(await stopSidecar(async () => true)).toBe(true);
    expect(await stopSidecar(async () => false)).toBe(false);
  });
});
