// Inline 24x24 line icons (stroke = currentColor). Kept tiny and dependency-free.

const paths: Record<string, string> = {
  terminal: '<rect x="3" y="4" width="18" height="16" rx="2.2"/><path d="M7 9l3 3-3 3"/><path d="M13.5 15H17"/>',
  powershell: '<path d="M8 6l6 6-6 6"/><path d="M13 18h5"/>',
  cmd: '<rect x="3" y="4" width="18" height="16" rx="2.2"/><path d="M7 10l2.4 2-2.4 2"/><path d="M12.5 14H17"/>',
  pwsh: '<path d="M8 6l6 6-6 6"/><path d="M13 18h5"/><circle cx="18.5" cy="7" r="1.3" fill="currentColor" stroke="none"/>',
  bash: '<rect x="3" y="4" width="18" height="16" rx="2.2"/><path d="M7.5 9.5l2.2 2.5-2.2 2.5"/><path d="M12.5 14.5H16.5"/>',
  ssh: '<rect x="3" y="4" width="18" height="7" rx="1.6"/><rect x="3" y="13" width="18" height="7" rx="1.6"/><path d="M6.5 7.5h.01M6.5 16.5h.01"/><path d="M15 7.5h3M15 16.5h3"/>',
  telnet: '<circle cx="12" cy="12" r="9"/><path d="M3 12h18"/><path d="M12 3c2.6 2.4 4 5.6 4 9s-1.4 6.6-4 9c-2.6-2.4-4-5.6-4-9s1.4-6.6 4-9z"/>',
  plus: '<path d="M12 5v14M5 12h14"/>',
  close: '<path d="M6 6l12 12M18 6L6 18"/>',
  search: '<circle cx="11" cy="11" r="7"/><path d="M20 20l-3.5-3.5"/>',
  settings: '<path d="M4 8h10M18 8h2M4 16h4M12 16h8"/><circle cx="16" cy="8" r="2.2"/><circle cx="9" cy="16" r="2.2"/>',
  chevronDown: '<path d="M6 9l6 6 6-6"/>',
  bolt: '<path d="M13 3L5 13h5l-1 8 8-11h-5l1-7z"/>',
  server: '<rect x="4" y="4" width="16" height="7" rx="1.5"/><rect x="4" y="13" width="16" height="7" rx="1.5"/><path d="M8 7.5h.01M8 16.5h.01"/>',
  x: '<path d="M6 6l12 12M18 6L6 18"/>',
  refresh: '<path d="M20 11a8 8 0 1 0-.9 4.5"/><path d="M20 5v6h-6"/>',
  folder: '<path d="M4 6a2 2 0 0 1 2-2h3.2a2 2 0 0 1 1.6.8L12 6h6a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2z"/>',
  pencil: '<path d="M4 20h4L18 10l-4-4L4 16z"/><path d="M13.5 6.5l4 4"/>',
  trash: '<path d="M4 7h16"/><path d="M9 7V5a1 1 0 0 1 1-1h4a1 1 0 0 1 1 1v2"/><path d="M6.5 7l.8 12a2 2 0 0 0 2 1.9h5.4a2 2 0 0 0 2-1.9l.8-12"/>',
  copy: '<rect x="9" y="9" width="11" height="11" rx="2"/><path d="M5 15V6a2 2 0 0 1 2-2h9"/>',
  shield: '<path d="M12 3l7 3v5.2c0 4.3-2.9 7.5-7 8.8-4.1-1.3-7-4.5-7-8.8V6z"/><path d="M9.2 12l1.9 1.9 3.7-3.9"/>',
  folderPlus: '<path d="M4 6a2 2 0 0 1 2-2h3.2a2 2 0 0 1 1.6.8L12 6h6a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2z"/><path d="M12 11v5M9.5 13.5h5"/>',
  files: '<rect x="3" y="4" width="7.5" height="16" rx="1.5"/><rect x="13.5" y="4" width="7.5" height="16" rx="1.5"/><path d="M6 8h1.5M6 12h1.5M16.5 8H18M16.5 12H18"/>',
  file: '<path d="M6 3h8l4 4v14H6z"/><path d="M14 3v5h5"/>',
  image: '<rect x="3" y="4" width="18" height="16" rx="2"/><circle cx="8.5" cy="9" r="1.5"/><path d="M4.5 17l5-5 3.3 3.3 2.2-2.2 4.5 4.4"/>',
  link: '<path d="M9.5 14.5l5-5"/><path d="M7 17H5.5a3.5 3.5 0 0 1 0-7H9M15 7h3.5a3.5 3.5 0 0 1 0 7H15"/>',
  back: '<path d="M15 5l-7 7 7 7"/>',
  forward: '<path d="M9 5l7 7-7 7"/>',
  up: '<path d="M5 15l7-7 7 7"/>',
  home: '<path d="M3 11l9-8 9 8"/><path d="M5.5 9.5V21h13V9.5M9.5 21v-6h5v6"/>',
  eye: '<path d="M2.5 12s3.5-6 9.5-6 9.5 6 9.5 6-3.5 6-9.5 6-9.5-6-9.5-6z"/><circle cx="12" cy="12" r="2.5"/>',
  move: '<path d="M12 3v18M3 12h18"/><path d="M8 7l4-4 4 4M8 17l4 4 4-4M7 8l-4 4 4 4M17 8l4 4-4 4"/>',
  more: '<circle cx="5" cy="12" r="1" fill="currentColor" stroke="none"/><circle cx="12" cy="12" r="1" fill="currentColor" stroke="none"/><circle cx="19" cy="12" r="1" fill="currentColor" stroke="none"/>',
  check: '<path d="M5 12.5l4.2 4.2L19 7"/>',
  winMin: '<path d="M5 12h14"/>',
  winMax: '<rect x="5.5" y="5.5" width="13" height="13" rx="1.5"/>',
  winRestore: '<rect x="8" y="8" width="11" height="11" rx="1.5"/><path d="M5 15.5V6.5A1.5 1.5 0 0 1 6.5 5h9"/>',
};

export function icon(name: keyof typeof paths | string, className = ""): string {
  const body = paths[name] ?? paths.terminal;
  return `<svg class="icon ${className}" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${body}</svg>`;
}

export type IconName = keyof typeof paths;
