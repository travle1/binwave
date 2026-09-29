import test from 'node:test';
import assert from 'node:assert/strict';
import vm from 'node:vm';
import {readFile} from 'node:fs/promises';
const source=(await readFile(new URL('../web/file-worker.mjs',import.meta.url),'utf8'))
    .replace(/^import .*\n/gm,'').replaceAll('import.meta.url',JSON.stringify('https://localhost/web/file-worker.mjs'));
const awaitedSettings = await readFile(new URL('../web/cache-settings.js',import.meta.url),'utf8');
function harness() {
    const workers=[],messages=[];
    class FileBackend {
        constructor(file,handle=null) {this.file=file;this.handle=handle;this.size=BigInt(file.size);this.originalSize=this.size;this.pieces=[];this.undo=[];this.redo=[];}
        async validate() {}
        async replace(offset,value) {this.pieces=[{kind:'added',at:BigInt(offset),bytes:[value]}];}
        async insert(offset,bytes) {this.size+=BigInt(bytes.length);this.pieces=[{kind:'added',at:BigInt(offset),bytes}];this.dirty=true;}
        async remove(start,end) {this.size-=BigInt(end)-BigInt(start);this.dirty=true;}
        async replaceSource() {this.replaced=true;}
    }
    class Worker {
        constructor() {this.terminated=false;this.commands=[];workers.push(this);}
        terminate(){this.terminated=true;}
        postMessage(m) {
            this.commands.push(m);
            if(m.yaml==='slow' || m.type==='cache_settings')return;
            queueMicrotask(()=>this.onmessage({data:m.yaml==='invalid' ? {type:'error',message:'Invalid YAML'} : {type:['decode','decode_python'].includes(m.type)?'decoded':m.type,result:{path:m.path}}}));
        }
    }
    const self={postMessage:m=>messages.push(m)};
    const edits = {memory: pieces => pieces.length};
    const context = vm.createContext({FileBackend,Worker,self,URL,PAGE_SIZE:65536,performance,edits});
    vm.runInContext(awaitedSettings, context);
    vm.runInContext(source, context);
    return {workers,messages,send: m=>self.onmessage({data:m})};
}
test('failed candidate preserves the previous decoder; edits invalidate it',async()=>{
    const h=harness();
    await h.send({type:'open',file:{name:'test.bin',size:40*1024**3}});
    await h.send({type:'decode',yaml:'valid'});
    const first=h.workers[0];
    await h.send({type:'decode',yaml:'invalid'});
    assert.equal(first.terminated,false);
    assert.equal(h.workers[1].terminated,true);
    await h.send({type:'table',path:'samples',start:'0'});
    assert.equal(first.commands.at(-1).type,'table');
    const probe={type:'probe',path:'samples',start:'4294967297',end:'4294967298',request:'7'};
    await h.send(probe);
    assert.deepEqual(first.commands.at(-1),probe,'Byte selection must reach the retained decoder intact');
    assert.equal(h.messages.at(-2).type,'probe');
    const pick={type:'pick',path:'samples',index:'536870912',request:'8'};
    await h.send(pick);
    assert.deepEqual(first.commands.at(-1),pick);
    assert.equal(h.messages.at(-2).type,'pick');
    await h.send({type:'replace',offset:'4294967297',value:9});
    assert.equal(first.terminated,true);
    await h.send({type:'table',path:'samples',start:'0'});
    assert.match(h.messages.at(-2).message,/Apply the decoder again/);
    await h.send({type:'decode',yaml:'valid'});
    assert.equal(h.workers.at(-1).commands[0].pieces[0].at,4294967297n, 'decoders read the edited pieces');
});
test('browser Python applies as a candidate and shares subsequent result requests',async()=>{
    const h=harness();
    await h.send({type:'open',file:{name:'test.bin',size:4}});
    await h.send({type:'decode_python',script:'from binwave import Struct'});
    assert.equal(h.workers.length,1);
    assert.equal(h.workers[0].commands[0].type,'decode_python');
    assert.equal(h.messages.at(-2).type,'decoded');
    await h.send({type:'inspect',start:'0',end:'1',request:'1'});
    assert.equal(h.workers[0].commands.at(-1).type,'inspect');
});
test('cancelling a candidate terminates work and retains the prior session',async()=>{
    const h=harness();
    await h.send({type:'open',file:{name:'test.bin',size:100}});
    await h.send({type:'decode',yaml:'valid'});
    const pending=h.send({type:'decode',yaml:'slow'});
    await new Promise(resolve=>setImmediate(resolve));
    await h.send({type:'cancel'});
    await pending;
    assert.equal(h.workers[0].terminated,false);
    assert.equal(h.workers[1].terminated,true);
    assert.match(h.messages.at(-2).message,/cancelled/);
    await h.send({type:'plot',path:'samples',start:'0',end:'10',width:800});
    assert.equal(h.workers[0].commands.at(-1).type,'plot');
});

test('RAM budget updates reach retained, pending, and future decoder sessions', async () => {
    const h = harness();
    await h.send({type:'cache_settings', cacheBytes:2 * 1024**3});
    await h.send({type:'open',file:{name:'synthetic.bin',size:1024}});
    await h.send({type:'decode',yaml:'valid'});
    assert.equal(h.workers[0].commands[0].cacheBytes, 2 * 1024**3);
    const pending = h.send({type:'decode',yaml:'slow'});
    await new Promise(resolve => setImmediate(resolve));
    await h.send({type:'cache_settings',cacheBytes:0});
    assert.equal(h.workers[0].commands.at(-1).cacheBytes, 0);
    assert.equal(h.workers[1].commands.at(-1).cacheBytes, 0);
    await h.send({type:'cancel'});
    await pending;
    await h.send({type:'decode_python',script:'pass'});
    assert.equal(h.workers.at(-1).commands[0].cacheBytes, 0);
});
test('inserts and deletes report the new size, and an in-place save continues on the saved file',async()=>{
    const h=harness();
    const saved={name:'data.bin',size:12};
    const handle={getFile:async()=>saved};
    await h.send({type:'open',file:{name:'data.bin',size:10},handle});
    await h.send({type:'decode',yaml:'valid'});
    await h.send({type:'insert',offset:'2',bytes:[1,2,3]});
    let edited=h.messages.findLast(m=>m.type==='edited');
    assert.equal(edited.size,'13');
    assert.equal(edited.savedSize,'10');
    assert.equal(h.workers[0].terminated,true,'a size change invalidates the decoder');
    await h.send({type:'delete',start:'0',end:'1'});
    assert.equal(h.messages.findLast(m=>m.type==='edited').size,'12');
    await h.send({type:'save_in_place'});
    edited=h.messages.findLast(m=>m.type==='edited');
    assert.equal(edited.saved,true);
    assert.equal(edited.size,'12');
    assert.equal(edited.savedSize,'12');
    assert.equal(edited.dirty,undefined,'the reopened file has no unsaved edits');
});
