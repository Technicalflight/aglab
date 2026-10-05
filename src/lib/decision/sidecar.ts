/**
 * Laya sidecar 的进程控制。
 *
 * 起进程只能由原生做（WebView 里没有任何这条途径），所以这里是一层薄薄的 invoke；
 * 命令名写成字面量是 lib.rs 那条命令面针的要求（它看不见变量透传）。
 *
 * 端口解析留在前端这一侧是有理由的：配置里的 `laya.sidecarEndpoint` 才是"app 会去敲
 * 哪个门"的唯一真相。启动参数必须照着它给，否则一键启动会起来一个连不上的进程——
 * 端口对不上时，面板上那句"已在运行"和"起不来"都是假话。
 */
import { platformInvoke } from "./providers/invoke";

/**
 * 从服务商里取端口。取不出（没写端口、写得不成样子）就返回 null，
 * 让原生用它自己的默认值，而不是在这里替用户编一个。
 *
 * IPv6 是这里唯一的坑：`http://[::1]:8787` 的最后一个冒号才是端口分隔符，
 * 而 `http://[::1]`（没写端口）冒号在方括号里面——那条切出来不是数字，落回 null。
 */
export function portOfEndpoint(endpoint: string): number | null {
  const raw = endpoint.trim();
  if (!raw) return null;
  const withoutScheme = raw.replace(/^[a-z][a-z0-9+.-]*:\/\//i, "");
  const authority = withoutScheme.split(/[/?#]/)[0];
  const colon = authority.lastIndexOf(":");
  if (colon < 0) return null;
  const port = Number(authority.slice(colon + 1));
  return Number.isInteger(port) && port > 0 && port <= 65535 ? port : null;
}

export interface SidecarLaunch {
  /** index.mjs 所在的那个目录 */
  dir: string;
  /** 配置里的 sidecarEndpoint：端口照着它给 */
  endpoint: string;
  subfolder?: string;
  invokeImpl?: (command: string, args: Record<string, unknown>) => Promise<unknown>;
}

/**
 * 起（或报回已经起着的那个的 pid）。失败抛的是原生那句人话——
 * "缺的是 node"还是"目录挑错了"，用户听得懂，也修得了
 */
export async function startSidecar(options: SidecarLaunch): Promise<number> {
  const invoke = options.invokeImpl ?? (await platformInvoke());
  const port = portOfEndpoint(options.endpoint);
  const pid = await invoke("decision_sidecar_start", {
    dir: options.dir,
    ...(port === null ? {} : { port }),
    ...(options.subfolder ? { subfolder: options.subfolder } : {}),
  });
  const value = Number(pid);
  if (!Number.isInteger(value) || value <= 0) {
    throw new Error(`sidecar 启动后没报回进程号，收到的是 ${String(pid)}`);
  }
  return value;
}

/** true = 收掉了 aglab 自己起的那个；false = 手上没有——那个是用户自己在终端里跑的 */
export async function stopSidecar(
  invokeImpl?: (command: string, args: Record<string, unknown>) => Promise<unknown>,
): Promise<boolean> {
  const invoke = invokeImpl ?? (await platformInvoke());
  return (await invoke("decision_sidecar_stop", {})) === true;
}
