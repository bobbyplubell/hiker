import { describe, it, expect, afterEach, vi } from "vitest";
import {
  openMenuAtAnchor,
  openMenuAtEvent,
  closeContextMenu,
  isContextMenuOpen,
} from "./contextMenu";

function makeAnchor(rect: Partial<DOMRect>): HTMLElement {
  const el = document.createElement("button");
  document.body.appendChild(el);
  // happy-dom returns a zeroed DOMRect by default; stub it so tests can
  // assert positioning math without laying anything out.
  el.getBoundingClientRect = () =>
    ({
      left: rect.left ?? 0,
      right: rect.right ?? 0,
      top: rect.top ?? 0,
      bottom: rect.bottom ?? 0,
      width: (rect.right ?? 0) - (rect.left ?? 0),
      height: (rect.bottom ?? 0) - (rect.top ?? 0),
      x: rect.left ?? 0,
      y: rect.top ?? 0,
      toJSON() {
        return {};
      },
    }) as DOMRect;
  return el;
}

afterEach(() => {
  closeContextMenu();
  document.body.innerHTML = "";
});

describe("openMenuAtAnchor", () => {
  it("positions the menu at the anchor's bottom-left by default", () => {
    const anchor = makeAnchor({ left: 100, right: 200, top: 50, bottom: 80 });
    openMenuAtAnchor(anchor, [{ label: "A", run: () => {} }]);
    const menu = document.querySelector(".ctx-menu") as HTMLElement;
    expect(menu).not.toBeNull();
    expect(menu.style.left).toBe("100px");
    expect(menu.style.top).toBe("80px");
  });

  it("aligns to the right edge when align='right'", () => {
    const anchor = makeAnchor({ left: 100, right: 200, top: 50, bottom: 80 });
    openMenuAtAnchor(anchor, [{ label: "A", run: () => {} }], { align: "right" });
    const menu = document.querySelector(".ctx-menu") as HTMLElement;
    expect(menu.style.left).toBe("200px");
  });

  it("applies offsetY below the anchor", () => {
    const anchor = makeAnchor({ left: 0, right: 50, top: 0, bottom: 30 });
    openMenuAtAnchor(anchor, [{ label: "A", run: () => {} }], { offsetY: 4 });
    const menu = document.querySelector(".ctx-menu") as HTMLElement;
    expect(menu.style.top).toBe("34px");
  });

  it("registers the anchor as the trigger for toggle-close semantics", () => {
    const anchor = makeAnchor({ left: 0, right: 0, top: 0, bottom: 0 });
    openMenuAtAnchor(anchor, [{ label: "A", run: () => {} }]);
    expect(isContextMenuOpen(anchor)).toBe(true);
    // Reopening with the same anchor toggles closed.
    openMenuAtAnchor(anchor, [{ label: "A", run: () => {} }]);
    expect(isContextMenuOpen()).toBe(false);
  });
});

describe("openMenuAtEvent", () => {
  it("places the menu at the event coordinates", () => {
    openMenuAtEvent({ clientX: 120, clientY: 240 }, [
      { label: "A", run: () => {} },
    ]);
    const menu = document.querySelector(".ctx-menu") as HTMLElement;
    expect(menu.style.left).toBe("120px");
    expect(menu.style.top).toBe("240px");
  });

  it("renders one row per item with the item label as text", () => {
    const run = vi.fn();
    openMenuAtEvent({ clientX: 0, clientY: 0 }, [
      { label: "First", run },
      { label: "Second", run: () => {} },
    ]);
    const items = document.querySelectorAll(".ctx-menu-item");
    expect(items.length).toBe(2);
    expect(items[0].textContent).toBe("First");
    expect(items[1].textContent).toBe("Second");
  });
});
