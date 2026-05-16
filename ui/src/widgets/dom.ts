/// `el()` is a tiny DOM construction helper. It collapses the repeated
/// "createElement / set className / set textContent / addEventListener /
/// appendChild" pattern into a single expression. Keep the option surface
/// small — anything exotic (RAF callbacks, IntersectionObserver, MutationObserver,
/// SVG via createElementNS) stays imperative at the call site.

export interface ElOpts {
  /// One or more space-separated class names.
  class?: string;
  /// Plain text content (mutually exclusive with `html`).
  text?: string;
  /// Raw inner HTML — only use for trusted markup (icons, never user input).
  html?: string;
  /// Element attributes set via setAttribute. Use this for `title`, `role`,
  /// `aria-*`, etc. Don't put `class` here — use the top-level `class` field.
  attrs?: Record<string, string | number | boolean | null | undefined>;
  /// Click handler shorthand. Maps to `addEventListener("click", ...)`.
  onClick?: (e: MouseEvent) => void;
  /// Other event listeners. Key is the event name.
  on?: Partial<{
    [E in keyof HTMLElementEventMap]: (ev: HTMLElementEventMap[E]) => void;
  }>;
  /// Style overrides (camelCase keys).
  style?: Partial<CSSStyleDeclaration>;
  /// True to set `hidden` on the element.
  hidden?: boolean;
  /// `dataset` shortcut — keys with hyphens map to camelCase here.
  data?: Record<string, string>;
  /// Set `title` (tooltip).
  title?: string;
  /// Set `disabled` on form controls.
  disabled?: boolean;
  /// Set `value` on inputs.
  value?: string | number | boolean;
  /// Set `id`.
  id?: string;
}

export function el<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  opts?: ElOpts,
  children?: (ChildNode | string | null | undefined | false)[],
): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag);
  if (opts?.class) node.className = opts.class;
  if (opts?.id) node.id = opts.id;
  if (opts?.title) node.title = opts.title;
  if (opts?.text !== undefined) node.textContent = opts.text;
  if (opts?.html !== undefined) node.innerHTML = opts.html;
  if (opts?.hidden) node.hidden = true;
  if (opts?.disabled !== undefined && "disabled" in node) {
    (node as HTMLButtonElement | HTMLInputElement).disabled = opts.disabled;
  }
  if (opts?.value !== undefined && "value" in node) {
    (node as HTMLInputElement).value = String(opts.value);
  }
  if (opts?.attrs) {
    for (const [k, v] of Object.entries(opts.attrs)) {
      if (v == null || v === false) continue;
      node.setAttribute(k, v === true ? "" : String(v));
    }
  }
  if (opts?.data) {
    for (const [k, v] of Object.entries(opts.data)) {
      node.dataset[k] = v;
    }
  }
  if (opts?.style) {
    Object.assign(node.style, opts.style);
  }
  if (opts?.onClick) {
    node.addEventListener("click", opts.onClick as unknown as EventListener);
  }
  if (opts?.on) {
    for (const [k, v] of Object.entries(opts.on)) {
      if (v) node.addEventListener(k, v as unknown as EventListener);
    }
  }
  if (children) {
    for (const c of children) {
      if (c == null || c === false) continue;
      node.appendChild(typeof c === "string" ? document.createTextNode(c) : c);
    }
  }
  return node;
}
