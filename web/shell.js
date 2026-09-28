// Browser chrome uses the same commands as the egui workspace. File pickers
// remain in bridge.js so their click handlers keep browser user activation.
(() => {
    const settings = document.getElementById('settings-dialog');
    const cacheInput = document.getElementById('file-cache-mib');
    const cacheError = document.getElementById('cache-settings-error');
    document.getElementById('sidebar-settings').onclick = () => {
        cacheInput.value = window.binwave.cacheBudgetMiB();
        cacheError.hidden = true;
        if (!settings.open) settings.showModal();
        cacheInput.focus();
    };
    document.getElementById('settings-close').onclick = () => settings.close();
    document.getElementById('cache-settings-default').onclick = () => {
        cacheInput.value = globalThis.binwaveCacheSettings.DEFAULT_BYTES / globalThis.binwaveCacheSettings.MIB;
        cacheError.hidden = true;
    };
    document.getElementById('cache-settings-form').onsubmit = event => {
        event.preventDefault();
        try {
            window.binwave.setCacheBudgetMiB(cacheInput.value);
            settings.close();
        } catch (error) {
            cacheError.textContent = error.message;
            cacheError.hidden = false;
        }
    };
    const sidebar = document.getElementById('sidebar');
    const expand = document.getElementById('expand-sidebar');
    const headerOpen = document.getElementById('header-open');
    const search = document.getElementById('search-dialog');
    const searchInput = document.getElementById('search-input');
    const searchResults = document.getElementById('search-results');
    const library = document.getElementById('decoder-library');
    const fileTitle = document.getElementById('file-title');
    const badge = document.getElementById('decode-badge');
    const recent = document.getElementById('recent-files');
    const recentHeading = document.getElementById('recent-heading');
    let stored;
    try { stored = localStorage.getItem('binwave-sidebar-open'); } catch { /* private browsing */ }
    let sidebarOpen = stored !== 'false';
    function setSidebar(open) {
        sidebarOpen = open;
        sidebar.hidden = !open;
        expand.hidden = headerOpen.hidden = open;
        try { localStorage.setItem('binwave-sidebar-open', String(open)); } catch { /* private browsing */ }
    }
    setSidebar(sidebarOpen && window.innerWidth >= 900);
    document.getElementById('collapse-sidebar').onclick = () => setSidebar(false);
    expand.onclick = () => setSidebar(true);
    headerOpen.onclick = () => window.binwaveMenus.invoke('open_file');
    document.getElementById('sidebar-open').onclick = () => window.binwaveMenus.invoke('open_file');
    document.getElementById('header-export').onclick = () => window.binwaveMenus.invoke('save_copy');
    fileTitle.onclick = () => window.binwaveMenus.invoke('open_file');
    function showSearch() {
        if (search.open) return;
        searchInput.value = '';
        renderSearch();
        search.showModal();
        searchInput.focus();
    }
    function renderSearch() {
        const query = searchInput.value.trim().toLowerCase();
        const commands = window.binwaveMenus.commands?.() || [];
        const matches = commands.filter(c => c.enabled && (!query || c.label.toLowerCase().includes(query)))
            .filter((c, i, all) => all.findIndex(item => item.command === c.command && item.argument === c.argument) === i)
            .slice(0, 30);
        searchResults.replaceChildren(...matches.map(command => {
            const button = document.createElement('button');
            button.textContent = command.label;
            button.onclick = () => { search.close(); window.binwaveMenus.invoke(command.command, command.argument); };
            return button;
        }));
    }
    document.getElementById('sidebar-search').onclick = showSearch;
    document.getElementById('search-close').onclick = () => search.close();
    searchInput.oninput = renderSearch;
    searchInput.onkeydown = event => {
        if (event.key === 'Enter') { searchResults.querySelector('button')?.click(); event.preventDefault(); }
    };
    const examples = [
        ['Waveform', 'waveform.yaml'], ['RIFF chunks', 'riff_chunks.yaml'],
        ['PNG chunks', 'png_chunks.yaml'], ['Nested records', 'nested_records.yaml'],
        ['Generic sections', 'generic_sections.yaml'], ['Generic primitives', 'generic_primitives.yaml'],
        ['Sensors', 'sensors.yaml'],
    ];
    const libraryItems = document.getElementById('library-items');
    const librarySearch = document.getElementById('library-search');
    const libraryOpen = document.getElementById('library-open');
    let selectedExample = null;
    let selectionRequest = 0;
    function renderLibrary() {
        const query = librarySearch.value.trim().toLowerCase();
        libraryItems.replaceChildren(...examples.filter(([label, name]) =>
            !query || `${label} ${name}`.toLowerCase().includes(query)).map(([label, name]) => {
            const button = document.createElement('button');
            button.textContent = `${label}  ·  YAML`;
            button.classList.toggle('selected', selectedExample?.name === name);
            button.onclick = async () => {
                const request = ++selectionRequest;
                for (const item of libraryItems.querySelectorAll('button')) item.classList.remove('selected');
                button.classList.add('selected');
                document.getElementById('library-name').textContent = label;
                document.getElementById('library-description').textContent = name;
                document.getElementById('library-preview').textContent = 'Loading preview…';
                libraryOpen.disabled = true;
                try {
                    const response = await fetch(`decoders/${name}`);
                    if (!response.ok) throw Error(`Could not load ${name}`);
                    const source = await response.text();
                    if (source.length > 256 * 1024) throw Error('Decoder is too large to preview');
                    if (request !== selectionRequest) return;
                    selectedExample = {name, source};
                    document.getElementById('library-preview').textContent = source;
                    libraryOpen.disabled = false;
                } catch (error) {
                    if (request !== selectionRequest) return;
                    selectedExample = null;
                    document.getElementById('library-preview').textContent = String(error.message || error);
                }
            };
            return button;
        }));
    }
    renderLibrary();
    librarySearch.oninput = renderLibrary;
    libraryOpen.onclick = () => {
        if (selectedExample && window.binwave.loadDecoderSource(selectedExample.name, selectedExample.source)) library.close();
    };
    document.getElementById('sidebar-library').onclick = () => library.showModal();
    document.getElementById('library-close').onclick = () => library.close();
    document.getElementById('library-import').onclick = () => {
        library.close(); window.binwaveMenus.invoke('open_decoder');
    };
    document.getElementById('library-new').onclick = () => {
        library.close(); window.binwaveMenus.invoke('new_decoder');
    };
    document.addEventListener('keydown', event => {
        if (!(event.ctrlKey || event.metaKey)) return;
        const key = event.key.toLowerCase();
        if (key === 'b') { event.preventDefault(); setSidebar(!sidebarOpen); }
        if (key === 'k') { event.preventDefault(); showSearch(); }
        if (key === 'o') { event.preventDefault(); window.binwaveMenus.invoke('open_file'); }
    });
    window.binwaveShell = {
        update(file, message) {
            const [name, size] = file.split(' · ');
            fileTitle.firstChild.textContent = name || 'No file open ';
            const context = document.getElementById('file-context');
            context.textContent = size ? size.replace(/ · unsaved changes$/, '') : '';
            const exportButton = document.getElementById('header-export');
            exportButton.disabled = !window.binwaveMenus.commands().some(c => c.command === 'save_copy' && c.enabled);
            badge.hidden = !file || !/decoded|partial/i.test(message);
            badge.classList.toggle('partial', /partial/i.test(message));
            badge.textContent = /partial/i.test(message) ? 'Partial decode' : 'Ready';
        },
        recent(names) {
            recentHeading.hidden = names.length === 0;
            recent.replaceChildren(...names.map((name, index) => {
                const button = document.createElement('button');
                const icon = document.createElement('span');
                icon.className = 'phosphor';
                icon.setAttribute('aria-hidden', 'true');
                icon.textContent = '\uE230';
                const label = document.createElement('span');
                label.className = 'recent-name';
                label.textContent = name;
                button.append(icon, label);
                button.title = name;
                button.classList.toggle('active', index === 0);
                button.onclick = () => window.binwave.reopenRecent(index);
                return button;
            }));
        },
    };
})();
