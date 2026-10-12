import type { AppConfig, Project } from "@/types/chat";

/**
 * 当前该看哪个工作目录。
 *
 * **话题自己的归属优先**，话题没绑（或它绑的项目已经删了）时回落到应用级默认。
 * 这与后端 `tools` 真正落盘的根目录同一个来源：回合开始时后端从话题台账里读
 * 归属，所以"资源管理器在看的目录"必须与"工具落在哪个目录"是一句话。
 *
 * 曾经这里读的是 `config.activeProjectId`（应用级默认）。那是"新建会话默认绑哪儿"，
 * 不是"我正在看的这条会话在哪儿干活"——切过项目之后，会话列表里选中的那条属于
 * 老项目，资源管理器却跳到了新项目；反过来，像"应用级默认没绑、但这条话题绑了"
 * 这种情况，资源管理器会直接显示"未绑定工作目录"，而话题明明在某个目录里干活。
 */
export function effectiveProjectId(config: AppConfig, conversationProjectId: string): string {
  if (config.projects.some((project) => project.id === conversationProjectId)) {
    return conversationProjectId;
  }
  if (config.projects.some((project) => project.id === config.activeProjectId)) {
    return config.activeProjectId;
  }
  return "";
}

/** 解析出项目本身，顺手把上面的优先级走一遍 */
export function effectiveProject(config: AppConfig, conversationProjectId: string): Project | null {
  const id = effectiveProjectId(config, conversationProjectId);
  return config.projects.find((project) => project.id === id) ?? null;
}
