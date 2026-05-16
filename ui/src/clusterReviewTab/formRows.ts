// status: cluster-review-tab
// status: cluster-review-tab-config-section
//
// Form-row + inline-edit helpers extracted from `index.ts`. These are
// pure builders — they take a label / current value / change callback
// and return a freshly-built element. They do not close over any pane
// state, so they live as plain module functions consumed by `index.ts`.

export function renderTextRow(
  label: string,
  value: string,
  onChange: (v: string) => void,
): HTMLElement {
  const wrap = document.createElement("label");
  wrap.className = "crt-row";
  const lbl = document.createElement("span");
  lbl.className = "crt-row-label";
  lbl.textContent = label;
  wrap.appendChild(lbl);
  const inp = document.createElement("input");
  inp.type = "text";
  inp.className = "crt-input";
  inp.value = value;
  inp.addEventListener("input", () => onChange(inp.value));
  wrap.appendChild(inp);
  return wrap;
}

export function renderNumberRow(
  label: string,
  initial: string,
  min: number,
  step: number,
  onChange: (n: number, raw: string) => void,
): HTMLElement {
  const wrap = document.createElement("label");
  wrap.className = "crt-row";
  const lbl = document.createElement("span");
  lbl.className = "crt-row-label";
  lbl.textContent = label;
  wrap.appendChild(lbl);
  const inp = document.createElement("input");
  inp.type = "number";
  inp.className = "crt-input";
  inp.value = initial;
  inp.min = String(min);
  inp.step = String(step);
  inp.addEventListener("input", () => {
    const raw = inp.value;
    const n = Number(raw);
    if (Number.isFinite(n)) onChange(n, raw);
    else onChange(min, raw);
  });
  wrap.appendChild(inp);
  return wrap;
}

export function renderCheckboxRow(
  label: string,
  checked: boolean,
  onChange: (v: boolean) => void,
): HTMLElement {
  const wrap = document.createElement("label");
  wrap.className = "crt-row crt-row-checkbox";
  const inp = document.createElement("input");
  inp.type = "checkbox";
  inp.checked = checked;
  inp.addEventListener("change", () => onChange(inp.checked));
  wrap.appendChild(inp);
  const lbl = document.createElement("span");
  lbl.className = "crt-row-label";
  lbl.textContent = label;
  wrap.appendChild(lbl);
  return wrap;
}

export function renderRadioRow(
  label: string,
  options: { value: string; label: string }[],
  current: string,
  onChange: (v: string) => void,
): HTMLElement {
  const wrap = document.createElement("div");
  wrap.className = "crt-row";
  const lbl = document.createElement("span");
  lbl.className = "crt-row-label";
  lbl.textContent = label;
  wrap.appendChild(lbl);
  const group = document.createElement("div");
  group.className = "crt-radio-group";
  for (const o of options) {
    const rowLab = document.createElement("label");
    rowLab.className = "crt-radio";
    const inp = document.createElement("input");
    inp.type = "radio";
    inp.name = label;
    inp.value = o.value;
    inp.checked = o.value === current;
    inp.addEventListener("change", () => {
      if (inp.checked) onChange(o.value);
    });
    rowLab.appendChild(inp);
    rowLab.appendChild(document.createTextNode(" " + o.label));
    group.appendChild(rowLab);
  }
  wrap.appendChild(group);
  return wrap;
}

export function beginInlineEdit(
  el: HTMLElement,
  initial: string,
  commit: (v: string) => void,
): void {
  const input = document.createElement("input");
  input.type = "text";
  input.className = "crt-inline-edit";
  input.value = initial;
  const parent = el.parentElement;
  if (!parent) return;
  parent.replaceChild(input, el);
  input.focus();
  input.select();
  let done = false;
  const finish = (save: boolean) => {
    if (done) return;
    done = true;
    if (save) commit(input.value);
    else parent.replaceChild(el, input);
  };
  input.addEventListener("keydown", (e) => {
    if (e.key === "Enter") {
      e.preventDefault();
      finish(true);
    } else if (e.key === "Escape") {
      e.preventDefault();
      finish(false);
    }
  });
  input.addEventListener("blur", () => finish(true));
}
