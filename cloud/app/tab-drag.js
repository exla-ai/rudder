// Find the slot in the original layout. Once the strip has scrolled to an
// end, the pointer can reach the end slot even when it grabbed a tab off-center.
export function tabDragPosition({ items, from, width, right, grab, x }, bounds, viewport) {
  const { scrollLeft, scrollWidth, clientWidth } = viewport;
  const first = items[0].left;
  const last = right - width;
  const left = Math.max(first, Math.min(last, x - bounds.left + scrollLeft - grab));
  const center = left + width / 2;
  let to;
  if (left === first || (x <= bounds.left + 6 && scrollLeft < 1)) {
    to = 0;
  } else if (left === last || (x >= bounds.right - 6 && scrollWidth - clientWidth - scrollLeft < 1)) {
    to = items.length - 1;
  } else {
    to = items.reduce((count, item, i) => count + Number(i !== from && center > item.center), 0);
  }
  return { to, offset: left - items[from].left };
}
