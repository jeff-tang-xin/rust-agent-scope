import { create } from "zustand";
import { loadJson, saveJson } from "../storage";

export const POSTFIX_KEY = "openidea.postfix.v1";
const KEY = POSTFIX_KEY;

export interface PostfixSettings {
  enabled: boolean;
  /** Expand `expr.key` with Tab (IDEA's default "Expand templates with Tab"). */
  expandWithTab: boolean;
  /** Offer postfix templates in the completion list. */
  showInCompletion: boolean;
  /** Disabled template ids. */
  disabled: string[];
}

const DEFAULTS: PostfixSettings = { enabled: true, expandWithTab: true, showInCompletion: true, disabled: [] };

export const usePostfixSettings = create<PostfixSettings>(() => loadJson(KEY, DEFAULTS));

export function updatePostfixSettings(patch: Partial<PostfixSettings>) {
  usePostfixSettings.setState(patch);
  const { enabled, expandWithTab, showInCompletion, disabled } = usePostfixSettings.getState();
  saveJson(KEY, { enabled, expandWithTab, showInCompletion, disabled });
}

export function setPostfixTemplateEnabled(id: string, on: boolean) {
  const disabled = new Set(usePostfixSettings.getState().disabled);
  if (on) disabled.delete(id);
  else disabled.add(id);
  updatePostfixSettings({ disabled: [...disabled] });
}

/** Re-reads the settings (changed by another window). */
export function reloadPostfixSettings() {
  usePostfixSettings.setState(loadJson(KEY, DEFAULTS));
}
