import { fetchReview } from "@/lib/chat-transport";
import type { ReviewInfo } from "@/types/chat";

/** 会话级缓存：最近一次读到的 git 清单。变更请求页随切随用——以前每次挂载都
 *  从零拉起并闪"读取中"，快速来回切就是一次次白屏（对齐子助理页的进页即画） */
let reviewCache: ReviewInfo | null = null;
/** 起草过的说明同样留在会话级：切去别的分区再回来，稿子还在 */
let draftCache: string | null = null;
let warming = false;

/** 预热：应用壳起来就后台拉一次清单。review 读的是激活项目（review.rs 的
 *  workspace()），与打开哪条话题无关，启动时拉不会错对象。
 *  住在独立小模块里：App 壳引用预热时不会把整个变更请求页拖回首屏包 */
export function warmReviewCache() {
  if (reviewCache !== null || warming) return;
  warming = true;
  fetchReview()
    .then((info) => {
      reviewCache = info;
    })
    .catch(() => undefined)
    .finally(() => {
      warming = false;
    });
}

export function getReviewCache(): ReviewInfo | null {
  return reviewCache;
}

export function setReviewCache(info: ReviewInfo) {
  reviewCache = info;
}

export function getDraftCache(): string | null {
  return draftCache;
}

export function setDraftCache(draft: string) {
  draftCache = draft;
}
