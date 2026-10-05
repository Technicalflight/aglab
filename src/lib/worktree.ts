import { invoke } from "@tauri-apps/api/core";

/** 一个话题挂着的 Worktree。Rust 侧 worktree::WorktreeView 的镜像 */
export interface WorktreeInfo {
  conversationId: string;
  /** 独立工作树的绝对路径（app_data/worktrees/<话题id>） */
  dir: string;
  /** 基于所选分支新建的分支，改动都在它上面 */
  branch: string;
  /** 开树时基于的分支 */
  baseBranch: string;
  repoPath: string;
  /** 工作树里有未提交的改动 */
  dirty: boolean;
  changedFiles: number;
}

/** 分支选择器的数据。isRepo 为假 = 当前工作目录不是 git 仓库，前端隐藏控件；
 *  unborn = HEAD 指着 current 但还没有任何提交——那个分支尚未诞生，
 *  只是显示出来像存在，实际开不了 Worktree（先提交一次才有基点） */
export interface GitBranches {
  isRepo: boolean;
  current: string;
  branches: string[];
  unborn: boolean;
}

/** 勾选 Worktree：基于所选分支开一棵独立工作树并绑到这个话题上 */
export const worktreeAttach = (conversationId: string, baseBranch?: string) =>
  invoke<WorktreeInfo>("worktree_attach", { conversationId, baseBranch: baseBranch ?? null });

/** 摘掉工作树。树脏时 git 拒绝——要强删传 force=true（改动不可恢复） */
export const worktreeDetach = (conversationId: string, force?: boolean) =>
  invoke<void>("worktree_detach", { conversationId, force: force ?? false });

/** 这个话题现在挂着哪棵树。没挂返回 null */
export const worktreeStatus = (conversationId: string) =>
  invoke<WorktreeInfo | null>("worktree_status", { conversationId });

/** 话题生效仓库的分支清单（话题绑定项目优先，散对话回落激活项目；非仓库时 isRepo=false）。
 *  清单不含应用自管的 aglab/wt/* 分支——它们是 worktree 的账目，不是可选基座 */
export const worktreeBranches = (conversationId: string) =>
  invoke<GitBranches>("worktree_branches", { conversationId });
