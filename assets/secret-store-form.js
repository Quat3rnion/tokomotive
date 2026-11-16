// Show only fields for the selected store and authentication method.
(() => {
  if (window.tokoStoreForms) { return; }
  window.tokoStoreForms = true;
  const scan = () => document.querySelectorAll('[data-store-form]').forEach(form => {
    if (form.dataset.ready) { return; }
    form.dataset.ready = 'true';
    const sync = () => {
      const kind = form.elements.kind.value;
      form.querySelectorAll('[data-store-kind]').forEach(group => {
        group.hidden = group.disabled = group.dataset.storeKind !== kind;
      });
      const address = form.querySelector('[data-store-address]');
      address.hidden = kind === 'bws';
      address.querySelector('input').disabled = kind === 'bws';
      address.querySelector('input').required = kind !== 'bws';
      for (const [attr, name] of [['vault-mode', 'vault_auth'], ['gcp-mode', 'gcp_auth']]) {
        const selected = form.querySelector(`[name="${name}"]:checked`);
        form.querySelectorAll(`[data-${attr}]`).forEach(group => {
          group.hidden = group.disabled = !selected || group.getAttribute('data-' + attr) !== selected.value;
        });
      }
      const hosted = form.querySelector('[data-bws-hosted]');
      hosted.hidden = hosted.disabled = form.elements.bws_region.value !== 'self-hosted';
    };
    form.addEventListener('change', async e => {
      sync();
      if (!e.target.matches('[data-service-account-file]')) { return; }
      const file = e.target.files[0];
      const status = form.querySelector('[data-service-account-status]');
      e.target.setCustomValidity('');
      if (!file) { return; }
      form.elements.gcp_service_account.value = '';
      if (file.size > 1024 * 1024) { status.textContent = 'The key file must be at most 1 MiB.'; e.target.setCustomValidity(status.textContent); return; }
      try {
        const text = await file.text();
        JSON.parse(text);
        if (e.target.files[0] !== file) { return; }
        form.elements.gcp_service_account.value = text;
        status.textContent = 'Key file read. Select Save store to replace the saved key.';
      } catch (err) { if (e.target.files[0] === file) { status.textContent = 'Cannot read the service-account JSON file.'; e.target.setCustomValidity(status.textContent); } }
    });
    sync();
  });
  scan();
  document.addEventListener('htmx:afterSettle', scan);
})();
