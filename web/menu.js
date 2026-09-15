// Render the shared Rust command hierarchy. File actions stay on the DOM click
// stack so Safari and File System Access retain transient user activation.
(() => {
    let signature='', bindings=[], groups=[], dispatch, allow;
    const close = () => { for (const group of groups) group.open=false; };
    const shape = items => items.map(({label,command,argument,children}) => [label,command,argument,shape(children)]);
    function render(model, onCommand, enabled) {
        const root=document.getElementById('app-menus');
        dispatch=onCommand; allow=enabled;
        const next=JSON.stringify(shape(model));
        if (next!==signature) {
            signature=next; bindings=[]; groups=[];
            const build=(items,top=false) => items.map(item => {
                if (item.command) {
                    const button=document.createElement('button');
                    button.type='button'; button.textContent=item.label;
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
            root.replaceChildren(...build(model,true));
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
    window.binwaveMenus={render,refresh};
})();
