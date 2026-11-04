import { useState } from "react";

/** 右侧面板宽度（左缘拖拽调宽，localStorage 记忆，各面板独立）。
 *
 *  拖拽时动态测量手柄左邻（对话主区）的实际宽度，保证拖拽后对话区
 *  不小于 chatMin——「面板能拖多宽」由对话区剩余空间决定，而不是写死
 *  一个与屏幕宽无关的上限（小屏大屏都成立）。
 *
 *  @param key  localStorage 键（各面板独立记忆）
 *  @param def  默认宽度（无记忆值时）
 *  @param min  面板宽度下限
 *  @param max  面板宽度上限（硬上限；实际还受对话区保底约束）
 *  @param chatMin 对话区最小保底宽度（默认 420px）
 */
export function usePanelWidth(
  key: string,
  def: number,
  min: number,
  max: number,
  chatMin = 420,
) {
  const [w, setW] = useState(() => {
    const v = Number(localStorage.getItem(key));
    return v >= min && v <= max ? v : def;
  });
  /** 放在面板左缘的拖拽手柄 onMouseDown。手柄的左邻必须是对话主区。 */
  const startResize = (e: React.MouseEvent) => {
    e.preventDefault();
    const startX = e.clientX;
    const startW = w;
    const chatNow =
      (e.currentTarget as HTMLElement).previousElementSibling?.getBoundingClientRect()
        .width ?? 0;
    const maxW = Math.min(max, startW + chatNow - chatMin);
    const onMove = (ev: MouseEvent) => {
      // 面板在右侧：向左拖 = 变宽。
      const nw = Math.min(maxW, Math.max(min, startW - (ev.clientX - startX)));
      setW(nw);
      localStorage.setItem(key, String(nw));
    };
    const onUp = () => {
      window.removeEventListener("mousemove", onMove);
      window.removeEventListener("mouseup", onUp);
    };
    window.addEventListener("mousemove", onMove);
    window.addEventListener("mouseup", onUp);
  };
  return { w, startResize };
}
