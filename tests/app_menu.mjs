import test from 'node:test';
import assert from 'node:assert/strict';
import vm from 'node:vm';
import {readFileSync} from 'node:fs';
function harness(){
    class Element {
        children=[];disabled=false;open=false;focused=false;
        append(...nodes){this.children.push(...nodes);}
        replaceChildren(...nodes){this.children=nodes;}
        contains(node){return node===this||this.children.some(c=>c.contains(node));}
        focus(){this.focused=true;}
        setAttribute(name,value){this[name]=value;}
        get firstElementChild(){return this.children[0];}
    }
    const root=new Element(),listeners={},window={};
    const document={getElementById:()=>root,createElement:()=>new Element(),addEventListener:(key,fn)=>listeners[key]=fn};
    vm.runInNewContext(readFileSync(new URL('../web/menu.js',import.meta.url),'utf8'),{document,window});
    const calls=[];
    const render=(model)=>window.binwaveMenus.render(model,(...args)=>calls.push(args),(_,enabled)=>enabled);
    return {root,listeners,render,calls};
}
const item=(label,command,enabled=true)=>({label,command,argument:0,enabled,children:[]});
const group=(label,children)=>({label,command:null,argument:0,enabled:true,children});
test('menu state updates preserve the open menu and disable stale actions',()=>{
    const h=harness();
    const model=[group('Open data',[item('Open file...','open_file')]),group('View',[item('Hex','hex')])];
    h.render(model);
    const menu=h.root.children[0],button=menu.children[1].children[0].children[1].children[0];
    menu.open=true;h.render(model);
    assert.equal(h.root.children[0],menu);assert.equal(menu.open,true);
    model[0].children[0].enabled=false;h.render(model);button.onclick();assert.equal(h.calls.length,0);
    model[0].children[0].enabled=true;h.render(model);button.onclick();
    assert.deepEqual(h.calls,[['open_file',0]]);assert.equal(menu.open,false);
});
test('overflow menu closes on Escape with focus restored and on outside click',()=>{
    const h=harness();h.render([group('Edit',[item('Copy','copy')]),group('View',[item('Hex','hex')])]);
    assert.equal(h.root.children.length,1);
    const menu=h.root.children[0];menu.open=true;
    assert.equal(menu.firstElementChild['aria-label'],'More commands');
    h.listeners.keydown({key:'Escape'});assert.equal(menu.open,false);assert.equal(menu.firstElementChild.focused,true);
    menu.open=true;h.listeners.click({target:{}});assert.equal(menu.open,false);
});
