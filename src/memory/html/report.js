(() => {
  'use strict';
  const body = document.getElementById('object-rows');
  const rows = [...body.querySelectorAll('.object-row')];
  const search = document.getElementById('object-search');
  const service = document.getElementById('service-filter');
  const scope = document.getElementById('scope-filter');
  const sortButtons = [...document.querySelectorAll('[data-sort-key]')];
  let sortKey = 'reachable', direction = 'desc';
  const numeric = new Set(['entries', 'self', 'reachable']);
  const detail = row => document.getElementById(row.dataset.detail);
  function close(row) {
    detail(row).hidden = true;
    row.classList.remove('expanded');
    row.querySelector('.row-toggle').setAttribute('aria-expanded', 'false');
  }
  function render() {
    rows.sort((a, b) => {
      let result;
      if (numeric.has(sortKey)) {
        const x = Number(a.dataset[sortKey]), y = Number(b.dataset[sortKey]);
        if (x < 0 || y < 0) {
          if (x !== y) return x < 0 ? 1 : -1;
        }
        result = x - y;
      } else {
        const field = sortKey === 'service' ? 'name' : sortKey;
        result = a.dataset[field].localeCompare(b.dataset[field]);
      }
      return (direction === 'asc' ? result : -result) ||
        Number(a.dataset.order) - Number(b.dataset.order);
    });
    const query = search.value.trim().toLocaleLowerCase();
    let visible = 0;
    for (const row of rows) {
      close(row);
      row.hidden = (service.value && row.dataset.service !== service.value) ||
        (scope.value === 'module' && row.dataset.module !== 'true') ||
        (query && !row.dataset.search.toLocaleLowerCase().includes(query));
      if (!row.hidden) row.querySelector('.rank').textContent = ++visible;
      body.append(row, detail(row));
    }
    for (const button of sortButtons) {
      const active = button.dataset.sortKey === sortKey;
      button.textContent = button.textContent.replace(/[↑↓]/g, '').trim() +
        (active ? (direction === 'asc' ? ' ↑' : ' ↓') : '');
      if (active) button.parentElement.setAttribute('aria-sort', direction === 'asc' ? 'ascending' : 'descending');
      else button.parentElement.removeAttribute('aria-sort');
    }
    document.getElementById('match-count').textContent = visible + ' / ' + rows.length + ' 个候选对象';
    document.getElementById('no-matches').hidden = visible !== 0;
  }
  function toggle(row) {
    const open = detail(row).hidden;
    rows.forEach(close);
    if (open) {
      detail(row).hidden = false;
      row.classList.add('expanded');
      row.querySelector('.row-toggle').setAttribute('aria-expanded', 'true');
    }
  }
  for (const row of rows) row.addEventListener('click', () => toggle(row));
  search.addEventListener('input', render);
  service.addEventListener('change', render);
  scope.addEventListener('change', render);
  document.getElementById('reset-filters').addEventListener('click', () => {
    search.value = ''; service.value = ''; scope.value = scope.dataset.default;
    sortKey = 'reachable'; direction = 'desc'; render();
  });
  for (const button of sortButtons) button.addEventListener('click', () => {
    if (sortKey === button.dataset.sortKey) direction = direction === 'asc' ? 'desc' : 'asc';
    else { sortKey = button.dataset.sortKey; direction = numeric.has(sortKey) ? 'desc' : 'asc'; }
    render();
  });
  for (const time of document.querySelectorAll('time[data-unix]')) {
    const date = new Date(Number(time.dataset.unix) * 1000);
    if (!Number.isNaN(date.getTime())) {
      time.textContent = date.toLocaleString('zh-CN', {hour12: false});
      time.dateTime = date.toISOString();
    }
  }
  render();
})();
