// Render the shared Rust command hierarchy. File actions stay on the DOM click
// stack so Safari and File System Access retain transient user activation.
(() => {
    let signature='', bindings=[], groups=[], dispatch, allow, current=[];
    const close = () => { for (const group of groups) group.open=false; };
    const shape = items => items.map(({label,command,argument,checked,children}) => [label,command,argument,checked,shape(children)]);
    function render(model, onCommand, enabled) {
        const root=document.getElementById('app-menus');
        dispatch=onCommand; allow=enabled; current=model;
        const next=JSON.stringify(shape(model));
        if (next!==signature) {
            signature=next; bindings=[]; groups=[];
            const build=(items,top=false) => items.map(item => {
                if (item.command) {
                    const button=document.createElement('button');
                    button.type='button'; button.textContent=item.checked === undefined ? item.label : `${item.checked ? '✓' : '  '}  ${item.label}`;
                    bindings.push({button,command:item.command,argument:item.argument});
                    button.onclick=()=>{if(button.disabled)return; close();dispatch(item.command,item.argument);};
                    return button;
                }
                const group=document.createElement('details');
                group.className=top?'app-menu':'submenu';
                const summary=document.createElement('summary');summary.textContent=item.label;
                const panel=document.createElement('div');panel.className='menu-items';panel.append(...build(item.children));
                group.append(summary,panel);
                if (top) {groups.push(group);group.ontoggle=()=>{if(group.open)for(const other of groups)if(other!==group)other.open=false;};}
                return group;
            });
            const overflow=document.createElement('details');
            overflow.className='app-menu';
            const trigger=document.createElement('summary');
            trigger.textContent='⋯';
            trigger.setAttribute('aria-label','More commands');
            trigger.title='More commands';
            const panel=document.createElement('div');
            panel.className='menu-items';
            panel.append(...build(model));
            overflow.append(trigger,panel);
            groups.push(overflow);
            root.replaceChildren(overflow);
        }
        const states=new Map();
        const collect=items=>{for(const item of items){if(item.command)states.set(`${item.command}:${item.argument}`,item.enabled);collect(item.children);}};
        collect(model);
        for(const binding of bindings)binding.enabled=states.get(`${binding.command}:${binding.argument}`);
        refresh();
    }
    function refresh(){for(const b of bindings)b.button.disabled=!allow(b.command,b.enabled);}
    document.addEventListener('click',event=>{if(!document.getElementById('app-menus').contains(event.target))close();});
    document.addEventListener('keydown',event=>{if(event.key==='Escape'){const group=groups.find(g=>g.open);close();group?.firstElementChild.focus();}});
    function invoke(command, argument=0) {
        const item=bindings.find(b=>b.command===command && b.argument===argument);
        if (!item || item.button.disabled) return false;
        close(); dispatch(command,argument); return true;
    }
    function commands() {
        const found=[];
        const visit=items=>{for(const item of items){if(item.command)found.push({label:item.label,command:item.command,argument:item.argument,enabled:allow(item.command,item.enabled)});visit(item.children);}};
        visit(current);
        return found;
    }
    window.binwaveMenus={render,refresh,invoke,commands,close};
})();
