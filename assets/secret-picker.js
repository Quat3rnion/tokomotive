// The secret picker never receives a resolved value, only names and masks.
(() => {
  if (window.tokoSecretPicker) { return; }
  window.tokoSecretPicker = true;
  const start = root => root.querySelectorAll('[data-secret-picker]').forEach(p => {
    if (p.dataset.ready) { return; }
    p.dataset.ready = 'true';
    const find = s => p.querySelector(`[data-secret-${s}]`);
    const stores = find('stores'), items = find('items'), fields = find('fields');
    const search = find('search'), ref = find('reference'), status = find('status');
    const next = find('next');
    let path = '', cursor = '', token = null, generation = 0, timer;
    const options = (select, rows, empty) => {
      select.replaceChildren(new Option(empty, ''));
      rows.forEach(row => select.add(new Option(row.label, row.id)));
    };
    const request = async (url, init) => {
      const r = await fetch(url, {credentials: 'same-origin', ...init});
      if (!r.ok) { throw new Error('Secret unavailable. Check the store and its access policy. For bw serve, check that the vault is unlocked.'); }
      return r.json();
    };
    const load = async () => {
      const n = ++generation;
      ref.value = '';
      options(fields, [], 'Choose a secret first');
      next.disabled = true;
      if (!stores.value) { options(items, [], 'Choose a store first'); return; }
      status.textContent = 'Loading secrets...';
      try {
        const q = new URLSearchParams({store: stores.value, path, cursor, q: search.value});
        const page = await request('/admin/secrets/browse?' + q);
        if (n !== generation) { return; }
        options(items, page.items, 'Choose a secret');
        token = page.next;
        next.disabled = !token;
        status.textContent = `${page.items.length} choices on this page.`;
      } catch (e) { if (n === generation) { status.textContent = e.message; } }
    };
    p.addEventListener('toggle', async () => {
      if (!p.open || stores.options.length > 1) { return; }
      try {
        const rows = await request('/admin/secrets/stores');
        options(stores, rows.map(s => ({id: s.name, label: `${s.name} (${s.kind})`})), 'Choose a store');
        if (rows.length === 1) { stores.value = rows[0].name; load(); }
      } catch (e) { status.textContent = e.message; }
    });
    stores.addEventListener('change', () => { path = cursor = ref.value = ''; fields.replaceChildren(); load(); });
    search.addEventListener('input', () => { cursor = ''; clearTimeout(timer); timer = setTimeout(load, 200); });
    next.addEventListener('click', () => { cursor = token || ''; load(); });
    find('back').addEventListener('click', () => { path = cursor = search.value = ''; load(); });
    items.addEventListener('change', async () => {
      const selected = items.value;
      ref.value = '';
      if (!selected) { return; }
      if (selected.endsWith('/')) {
        path = selected.slice(selected.indexOf(':') + 1).replace(/\/$/, '');
        cursor = search.value = '';
        load(); return;
      }
      try {
        const list = await request('/admin/secrets/fields?' + new URLSearchParams({reference: selected}));
        if (items.value !== selected) { return; }
        options(fields, list.map(f => ({id: f ? 'field:' + f : 'whole:', label: f || '(whole value)'})), 'Choose a field');
        if (list.length === 1) { fields.selectedIndex = 1; fields.dispatchEvent(new Event('change')); }
        status.textContent = 'Choose a field, then select Test.';
      } catch (e) { status.textContent = e.message; }
    });
    fields.addEventListener('change', () => {
      ref.value = fields.value ? items.value + (fields.value === 'whole:' ? '' : '#' + fields.value.slice(6)) : '';
      status.textContent = ref.value ? 'Selected: ' + ref.value : 'Choose a field.';
    });
    find('test').addEventListener('click', async () => {
      if (!ref.value) { status.textContent = 'Choose a secret and a field first.'; return; }
      try {
        const value = await request('/admin/secrets/test', {method: 'POST', body: new URLSearchParams({reference: ref.value})});
        status.textContent = 'Resolved: ' + value.masked;
      } catch (e) { status.textContent = e.message; }
    });
  });
  start(document);
  document.addEventListener('htmx:afterSettle', () => start(document));
})();
