/**
 * 决策层的错误谱系。路由器靠 instanceof 分流：
 * Unavailable/Timeout = 换下一层重试；其他 DecisionError = 原样上抛。
 * 调用方只需要认识 DecisionUnavailableError 一种情况：整条漏斗都拿不到答案。
 */

export class DecisionError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "DecisionError";
  }
}

/**
 * 该层不可用：没配置、没启动、探测失败、HTTP 非 2xx。路由器的信号是「跳过，换下一层」。
 * `status`：provider 拿得到 HTTP 状态码时带上。链的降级判定吃它（§5.4 修 B7）——
 * 402/429/5xx 降级到下一跳，其余 4xx fail-fast；没带 = 网络层错误，同样降级。
 */
export class DecisionUnavailableError extends DecisionError {
  readonly status?: number;
  constructor(message: string, options?: { status?: number }) {
    super(message);
    this.name = "DecisionUnavailableError";
    this.status = options?.status;
  }
}

/** 超时单独成类：网络慢和服务商死了在运维上是两件事，审计里要能分开看 */
export class DecisionTimeoutError extends DecisionError {
  constructor(message: string) {
    super(message);
    this.name = "DecisionTimeoutError";
  }
}
