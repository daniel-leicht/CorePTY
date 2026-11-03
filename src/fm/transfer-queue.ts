// The transfer strip at the bottom of a file manager tab: one row per job with
// progress, speed and time left. Conflict questions from the backend are shown
// one at a time, only while their tab is in front, and can be put off: a job
// whose question was dismissed keeps waiting with an "Answer…" button.

import { icon } from "../icons";
import {
  transferApi,
  type TransferConflict,
  type TransferEvent,
  type TransferRequest,
} from "../ipc";
import { conflictDialog, type DialogContext } from "./dialogs";
import { formatDuration, formatFileSize, formatSpeed } from "./format";

type JobState =
  | "queued"
  | "planning"
  | "running"
  | "waiting"
  | "paused"
  | "done"
  | "failed"
  | "cancelled";

interface Job {
  id: string | null;
  request: TransferRequest;
  label: string;
  state: JobState;
  element: HTMLElement;
  totalItems: number;
  totalBytes: number;
  completedItems: number;
  completedBytes: number;
  skipped: number;
  speed: number;
  eta: number | null;
  current: string;
  message: string;
  /** The unanswered conflict question, if any. */
  question: { conflictId: number; conflict: TransferConflict } | null;
  /** The user dismissed the question: don't pop it up again by itself. */
  deferred: boolean;
}

export interface QueueHost {
  toast(message: string, kind?: "info" | "warn" | "error"): void;
  /** A job ended (done, failed or cancelled): refresh the panels it touched. */
  finished(request: TransferRequest): void;
  /** Something changed that the status bar shows. */
  changed(): void;
  /** Whether this tab is in front (questions only pop up then). */
  isActive(): boolean;
  /** Dialog ownership: dismissed when the tab closes; where focus returns. */
  dialogContext(): DialogContext;
  /** Give the keyboard back to the active panel. */
  focus(): void;
}

const DONE_ROW_MS = 5000;

export class TransferQueue {
  readonly element: HTMLElement;
  private readonly list: HTMLElement;
  private readonly jobs = new Set<Job>();
  /** The job whose question is on screen. */
  private asking: Job | null = null;
  private collapsed = false;

  constructor(private readonly host: QueueHost) {
    this.element = document.createElement("div");
    this.element.className = "fm-queue";
    this.element.hidden = true;
    this.element.innerHTML = `
      <button type="button" class="fm-queue__head" aria-expanded="true">
        <span class="fm-queue__title">Transfers</span><span class="fm-queue__summary"></span>${icon("chevronDown")}
      </button>
      <div class="fm-queue__list"></div>`;
    this.list = this.element.querySelector(".fm-queue__list")!;
    this.element.querySelector(".fm-queue__head")!.addEventListener("click", () => {
      this.collapsed = !this.collapsed;
      this.element.classList.toggle("is-collapsed", this.collapsed);
      this.element.querySelector(".fm-queue__head")!.setAttribute("aria-expanded", String(!this.collapsed));
    });
  }

  /** Queue a copy/move; returns once the backend accepted it. */
  async start(request: TransferRequest, label: string): Promise<void> {
    const job = this.createJob(request, label);
    try {
      const view = await transferApi.start(request, (event) => this.onEvent(job, event));
      job.id ??= view.id;
    } catch (error) {
      job.state = "failed";
      job.message = String(error);
      this.update(job);
      this.host.toast(`${label} failed: ${error}`, "error");
    }
  }

  /** The tab came to the front: ask any question that has been waiting. */
  resume(): void {
    this.askNext(false);
  }

  /** "2 transfers · 45%", or null when idle. */
  summary(): string | null {
    let active = 0;
    let done = 0;
    let total = 0;
    let waiting = 0;
    for (const job of this.jobs) {
      if (!isActive(job.state)) continue;
      active += 1;
      done += job.completedBytes;
      total += job.totalBytes;
      if (job.question) waiting += 1;
    }
    if (!active) return null;
    const percent = total ? ` · ${Math.floor((done / total) * 100)}%` : "";
    const question = waiting ? ` · ${waiting} waiting for you` : "";
    return `${active} ${active === 1 ? "transfer" : "transfers"}${percent}${question}`;
  }

  /** Cancel everything still running (the tab is closing). */
  cancelAll(): void {
    for (const job of this.jobs) {
      if (job.id && isActive(job.state)) void transferApi.cancel(job.id).catch(() => false);
    }
  }

  private createJob(request: TransferRequest, label: string): Job {
    const element = document.createElement("div");
    element.className = "fm-job";
    element.innerHTML = `
      <span class="fm-job__icon">${icon(request.kind === "copy" ? "copy" : "move")}</span>
      <div class="fm-job__text"><div class="fm-job__title"></div><div class="fm-job__detail"></div></div>
      <div class="fm-progress" role="progressbar" aria-valuemin="0" aria-valuemax="100" aria-valuenow="0"><span></span></div>
      <button type="button" class="btn ghost fm-job__answer" hidden>Answer…</button>
      <button type="button" class="fm-job__action" title="Cancel">${icon("close")}</button>`;
    element.querySelector(".fm-job__title")!.textContent = label;
    element.querySelector(".fm-progress")!.setAttribute("aria-label", label);
    const job: Job = {
      id: null,
      request,
      label,
      state: "queued",
      element,
      totalItems: 0,
      totalBytes: 0,
      completedItems: 0,
      completedBytes: 0,
      skipped: 0,
      speed: 0,
      eta: null,
      current: "",
      message: "",
      question: null,
      deferred: false,
    };
    element.querySelector(".fm-job__action")!.addEventListener("click", () => {
      if (isActive(job.state)) {
        if (job.id) void transferApi.cancel(job.id).catch(() => false);
      } else {
        this.remove(job);
      }
    });
    element.querySelector(".fm-job__answer")!.addEventListener("click", () => {
      job.deferred = false;
      this.ask(job);
    });
    this.jobs.add(job);
    this.list.prepend(element);
    this.element.hidden = false;
    this.update(job);
    return job;
  }

  private onEvent(job: Job, event: TransferEvent): void {
    job.id ??= event.id;
    switch (event.event) {
      case "queued":
        job.state = "queued";
        break;
      case "planning":
        job.state = "planning";
        break;
      case "planned":
        job.state = "running";
        job.totalItems = event.totalItems;
        job.totalBytes = event.totalBytes;
        break;
      case "progress":
        if (!job.question) job.state = "running";
        job.completedItems = event.completedItems;
        job.completedBytes = event.completedBytes;
        job.totalItems = event.totalItems;
        job.totalBytes = event.totalBytes;
        job.skipped = event.skippedItems;
        job.speed = event.bytesPerSecond;
        job.eta = event.etaSeconds;
        job.current = event.current;
        break;
      case "conflict":
        job.state = "waiting";
        job.question = { conflictId: event.conflictId, conflict: event.conflict };
        job.deferred = false;
        this.askNext(false);
        break;
      case "finished":
        job.state = "done";
        job.completedItems = event.completedItems;
        job.completedBytes = event.completedBytes;
        job.skipped = event.skippedItems;
        this.ended(job);
        break;
      case "failed":
        job.state = "failed";
        job.message = event.message;
        this.host.toast(`${job.label} failed: ${event.message}`, "error");
        this.ended(job);
        break;
      case "cancelled":
        job.state = "cancelled";
        this.ended(job);
        break;
      case "paused":
        // The connection dropped: it carries on (where it stopped) after a reconnect.
        job.state = "paused";
        job.message = event.message;
        job.speed = 0;
        job.eta = null;
        break;
    }
    this.update(job);
  }

  /** Show the next waiting question, if this tab is in front and none is open. */
  private askNext(includeDeferred: boolean): void {
    if (this.asking || !this.host.isActive()) return;
    for (const job of this.jobs) {
      if (job.question && (includeDeferred || !job.deferred)) {
        this.ask(job);
        return;
      }
    }
  }

  private ask(job: Job): void {
    const question = job.question;
    if (!question || this.asking || !job.id) return;
    this.asking = job;
    void (async () => {
      const answer = await conflictDialog(question.conflict, this.host.dialogContext());
      this.asking = null;
      if (answer && job.question === question && job.id) {
        job.question = null;
        job.state = "running";
        await transferApi.resolve(job.id, question.conflictId, answer).catch(() => undefined);
      } else if (!answer && job.question === question) {
        job.deferred = true; // answer later, from the job row
      }
      this.update(job);
      this.askNext(false);
    })();
  }

  private ended(job: Job): void {
    job.question = null;
    this.host.finished(job.request);
    if (job.state !== "failed") window.setTimeout(() => this.remove(job), DONE_ROW_MS);
  }

  private remove(job: Job): void {
    if (!this.jobs.delete(job)) return;
    const hadFocus = job.element.contains(document.activeElement);
    job.element.remove();
    this.element.hidden = this.jobs.size === 0;
    if (hadFocus) this.host.focus();
    this.renderSummary();
    this.host.changed();
  }

  private update(job: Job): void {
    const { element } = job;
    element.className = `fm-job is-${job.state}`;
    const ratio = job.totalBytes
      ? job.completedBytes / job.totalBytes
      : job.totalItems
        ? job.completedItems / job.totalItems
        : 0;
    const percent = Math.round(Math.min(1, job.state === "done" ? 1 : ratio) * 100);
    element.querySelector<HTMLElement>(".fm-progress span")!.style.width = `${percent}%`;
    element.querySelector(".fm-progress")!.setAttribute("aria-valuenow", String(percent));
    element.querySelector(".fm-job__detail")!.textContent = detail(job);
    element.querySelector<HTMLButtonElement>(".fm-job__answer")!.hidden = !job.question;
    const action = element.querySelector<HTMLButtonElement>(".fm-job__action")!;
    action.title = isActive(job.state) ? "Cancel" : "Dismiss";
    this.renderSummary();
    this.host.changed();
  }

  private renderSummary(): void {
    this.element.querySelector(".fm-queue__summary")!.textContent = this.summary() ?? "";
  }
}

function isActive(state: JobState): boolean {
  return state !== "done" && state !== "failed" && state !== "cancelled";
}

function detail(job: Job): string {
  const skipped = job.skipped ? ` · ${job.skipped} skipped` : "";
  switch (job.state) {
    case "queued":
      return "Waiting in the queue…";
    case "planning":
      return "Counting files…";
    case "waiting":
      if (!job.question) return "Waiting for your answer…";
      return job.question.conflict.partial
        ? `“${job.question.conflict.name}” was partly copied before — resume it?`
        : `“${job.question.conflict.name}” already exists — waiting for your answer`;
    case "done":
      return `Done · ${job.completedItems} ${job.completedItems === 1 ? "item" : "items"}${skipped}`;
    case "failed":
      return job.message;
    case "cancelled":
      return "Cancelled";
    case "paused":
      return `Paused — ${job.message}`;
    case "running": {
      const parts = [
        job.current,
        `${formatFileSize(job.completedBytes)} of ${formatFileSize(job.totalBytes)}`,
        formatSpeed(job.speed),
        job.eta !== null ? `${formatDuration(job.eta)} left` : "",
      ];
      return parts.filter(Boolean).join(" · ") + skipped;
    }
  }
}
