import { invoke } from "@tauri-apps/api/core";

/** 一个资料库的列表摘要（不含正文）。Rust 侧 knowledge::KbSummary 的镜像 */
export interface KbSummary {
  id: string;
  name: string;
  description: string;
  /** 空串 = 未绑定工作目录；非空 = config.projects 里的项目 id */
  projectId: string;
  docCount: number;
  chars: number;
  createdAt: number;
  updatedAt: number;
}

/** 文档的列表条目（不含正文）。Rust 侧 knowledge::KbDocMeta 的镜像 */
export interface KbDocMeta {
  id: string;
  title: string;
  /** 出处：导入文件是它的路径，手动录入是「手动录入」 */
  source: string;
  chars: number;
  createdAt: number;
  updatedAt: number;
}

/** 库详情：摘要 + 文档元数据列表。Rust 侧 knowledge::KbDetail（flatten）的镜像 */
export interface KbDetail extends KbSummary {
  docs: KbDocMeta[];
}

/** 文档全文。kb_doc_get 才给正文，列表与详情都不背内容 */
export interface KbDoc {
  id: string;
  title: string;
  source: string;
  content: string;
  createdAt: number;
  updatedAt: number;
}

/** 一条检索命中。Rust 侧 knowledge::KbHit 的镜像 */
export interface KbHit {
  kbId: string;
  kbName: string;
  docId: string;
  docTitle: string;
  snippet: string;
  score: number;
  updatedAt: number;
}

export interface KbImportOutcome {
  added: number;
  skipped: number;
  /** 每一条被跳过的原因，直接展示给用户 */
  skippedNames: string[];
}

export const kbList = () => invoke<KbSummary[]>("kb_list");

export const kbCreate = (name: string, description: string, projectId: string) =>
  invoke<KbSummary>("kb_create", { name, description, projectId });

export const kbUpdate = (id: string, name: string, description: string) =>
  invoke<KbSummary>("kb_update", { id, name, description });

export const kbDelete = (id: string) => invoke<void>("kb_delete", { id });

export const kbGet = (id: string) => invoke<KbDetail>("kb_get", { id });

export const kbDocAdd = (id: string, title: string, content: string, source?: string) =>
  invoke<KbDocMeta>("kb_doc_add", { id, title, content, source: source ?? null });

export const kbDocUpdate = (id: string, docId: string, title: string, content: string) =>
  invoke<KbDocMeta>("kb_doc_update", { id, docId, title, content });

export const kbDocDelete = (id: string, docId: string) => invoke<void>("kb_doc_delete", { id, docId });

export const kbDocGet = (id: string, docId: string) => invoke<KbDoc>("kb_doc_get", { id, docId });

/** 跨库检索。projectId 缺省 = 全部；limit 缺省由后端定（20） */
export const kbSearch = (query: string, projectId?: string, limit?: number) =>
  invoke<KbHit[]>("kb_search", { query, projectId: projectId ?? null, limit: limit ?? null });

export const kbImportFiles = (id: string, paths: string[]) =>
  invoke<KbImportOutcome>("kb_import_files", { id, paths });

// ---- 语义检索（embedding） ----

/** 语义索引状态。Rust 侧 knowledge::EmbedStatusView 的镜像 */
export interface KbEmbedStatus {
  enabled: boolean;
  model: string;
  indexedModel: string;
  chunks: number;
  /** true = 向量库还没建或模型不一致，点重建 */
  stale: boolean;
}

export const kbEmbedStatus = () => invoke<KbEmbedStatus>("kb_embed_status");

export const kbReembed = () => invoke<void>("kb_reembed");

export const kbImportWiki = (id: string, repo: string) =>
  invoke<KbImportOutcome>("kb_import_wiki", { id, repo });

/** 端点的模型目录（OpenAI 兼容 GET /models，密钥与代理走主连接）。设置页挑 embedding 模型用 */
export const embeddingModels = (baseUrl: string) =>
  invoke<string[]>("embedding_models", { baseUrl });

/** embedding 专用密钥：独立凭据槽，与主密钥/各档案槽隔离。只写不读，内容不回前端 */
export const embeddingCredentialSet = (secret: string) =>
  invoke<void>("embedding_credential_set", { secret });

export const embeddingCredentialClear = () => invoke<void>("embedding_credential_clear");

export const embeddingCredentialProbe = () => invoke<boolean>("embedding_credential_probe");

// ---- Umi-OCR 引擎（资料库导入 PDF/图片的文字提取）----

/** 引擎状态。Rust 侧 knowledge::ocr::EngineStatus 的镜像 */
export interface OcrEngineStatus {
  installed: boolean;
  running: boolean;
  /** 探到的版本串（如 "Umi-OCR v2.1.5"），没跑起来就是空 */
  version: string;
  downloading: boolean;
  enginePath: string;
}

export const ocrEngineStatus = () => invoke<OcrEngineStatus>("ocr_engine_status");

export const ocrEngineStart = () => invoke<void>("ocr_engine_start");

/** 下载官方 Paddle 整合包（约 134MB）并解压到应用数据目录。同一时刻只允许一个任务 */
export const ocrEngineDownload = () => invoke<void>("ocr_engine_download");

/** 对话导出：format = markdown | json | jsonl，path 来自保存对话框。返回实际写盘路径 */
export const exportConversation = (id: string, format: string, path: string) =>
  invoke<string>("export_conversation", { id, format, path });


// ---- 纯函数（vitest 钉在这里，不碰 invoke） ----

/** 工作目录过滤的取值："all" 全部 / "none" 未绑定 / 其余 = 项目 id */
export type KbWorkspaceFilter = "all" | "none" | string;

/** 列表的客户端过滤：工作目录在前、关键词在后（名称与描述都搜） */
export function filterKbs(items: KbSummary[], query: string, workspace: KbWorkspaceFilter): KbSummary[] {
  const keyword = query.trim().toLowerCase();
  return items.filter((item) => {
    const workspaceHit =
      workspace === "all" ||
      (workspace === "none" ? item.projectId === "" : item.projectId === workspace);
    if (!workspaceHit) return false;
    if (!keyword) return true;
    return (
      item.name.toLowerCase().includes(keyword) ||
      item.description.toLowerCase().includes(keyword)
    );
  });
}

/** 字数的人话。过万就折万——"123456 字" 读起来太费劲 */
export function formatChars(chars: number): string {
  if (chars >= 10_000) return `${(chars / 10_000).toFixed(1)} 万字`;
  return `${chars} 字`;
}

/** 相对时间。与侧栏话题列表同一套口径：刚刚 / N 分钟前 / N 小时前 / M月D日 */
export function relativeTime(timestamp: number): string {
  if (!timestamp) return "—";
  const minutes = Math.floor((Date.now() - timestamp) / 60_000);
  if (minutes < 1) return "刚刚";
  if (minutes < 60) return `${minutes} 分钟前`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours} 小时前`;
  const date = new Date(timestamp);
  return `${date.getMonth() + 1}月${date.getDate()}日`;
}
