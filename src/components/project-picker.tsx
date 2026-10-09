import {
  IconCheck as Check,
  IconFolder as Folder,
  IconFolderPlus as FolderPlus,
} from "@tabler/icons-react";

import {
  Menu,
  MenuContent,
  MenuItem,
  MenuLabel,
  MenuSeparator,
  MenuTrigger,
} from "@/components/ui/menu";
import { ProjectDialog } from "@/components/project-dialog";
import { useChatStore } from "@/store/chat-store";

export function ProjectPicker() {
  const projects = useChatStore((s) => s.config.projects);
  // 显示的是**当前话题**的归属，不是应用级默认——选择器长在输入框旁边，
  // 人读它当"这条话题绑的是哪"。两处各读一份的话，先建话题再解绑的人会看到
  // "选择工作目录"、话题却排在老项目下面（同一件事两种说法）
  const conversationProjectId = useChatStore((s) => s.projectId);
  const chooseProject = useChatStore((s) => s.chooseProject);
  const dialogOpen = useChatStore((s) => s.projectDialogOpen);
  const setDialogOpen = useChatStore((s) => s.setProjectDialogOpen);

  const active = projects.find((project) => project.id === conversationProjectId);

  return (
    <>
      <Menu>
        <MenuTrigger
          type="button"
          className="flex h-7 items-center gap-1.5 rounded-lg px-2 text-sm text-muted-foreground outline-none transition-colors hover:bg-accent hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring/45 data-[state=open]:bg-accent"
        >
          <Folder className="size-3.5" />
          {active ? active.name : "选择工作目录"}
        </MenuTrigger>

        <MenuContent>
          <MenuLabel>工作目录</MenuLabel>

          {projects.map((project) => (
            <MenuItem
              key={project.id}
              onSelect={() => void chooseProject(project.id)}
              className="justify-between gap-3"
            >
              <span className="min-w-0 flex-1">
                <span className="block truncate">{project.name}</span>
                <span className="block truncate font-mono text-xs text-muted-foreground">
                  {project.path}
                </span>
              </span>
              {project.id === conversationProjectId ? (
                <Check className="size-3.5 shrink-0 text-brand-text" />
              ) : null}
            </MenuItem>
          ))}

          {projects.length > 0 ? (
            <MenuItem onSelect={() => void chooseProject("")}>
              不绑定工作目录（文件工具以主目录为基准）
            </MenuItem>
          ) : null}

          <MenuSeparator />

          <MenuItem onSelect={() => setDialogOpen(true)}>
            <FolderPlus className="size-3.5 text-muted-foreground" />
            新建工作目录
          </MenuItem>
        </MenuContent>
      </Menu>

      <ProjectDialog open={dialogOpen} onOpenChange={setDialogOpen} />
    </>
  );
}
