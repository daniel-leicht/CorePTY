/** Shared lifecycle and presentation contract for every first-class workspace tab. */
export type TabStatus = "connecting" | "connected" | "exited" | "error";

export interface TabStatusInfo {
  kind: string;
  context?: string;
  metrics?: string;
}

export interface AppTab {
  readonly tabType: "terminal" | "file-manager";
  readonly uid: string;
  readonly element: HTMLDivElement;
  readonly elevated: boolean;
  readonly pinned: boolean;
  readonly title: string;
  readonly iconName: string;
  readonly status: TabStatus;

  onTitleUpdate?: () => void;
  onStatusChange?: () => void;
  onClose?: () => void;

  setCustomTitle(name: string | null): void;
  statusInfo(): TabStatusInfo;
  open(): void;
  fit(): void;
  focus(): void;
  applySettings(): void;
  dispose(): void;
}
