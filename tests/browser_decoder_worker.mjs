import test from 'node:test';
import assert from 'node:assert/strict';
import vm from 'node:vm';
import {readFile} from 'node:fs/promises';
const source=(await readFile(new URL('../web/file-worker.mjs',import.meta.url),'utf8'))
    .replace(/^import .*\n/,'').replaceAll('import.meta.url',JSON.stringify('https://localhost/web/file-worker.mjs'));
function harness() {
    const workers=[],messages=[];
    class FileBackend {
        constructor(file) {this.file=file;this.size=BigInt(file.size);this.patches=new Map();this.undo=[];this.redo=[];}
        async validate() {}
        async replace(offset,value) {this.patches.set(BigInt(offset),value);}
    }
    class Worker {
        constructor() {this.terminated=false;this.commands=[];workers.push(this);}
        terminate(){this.terminated=true;}
        postMessage(m) {
            this.commands.push(m);
            if(m.yaml==='slow')return;
            queueMicrotask(()=>this.onmessage({data:m.yaml==='invalid' ? {type:'error',message:'Invalid YAML'} : {type:m.type==='decode'?'decoded':m.type,result:{path:m.path}}}));
        }
    }
    const self={postMessage:m=>messages.push(m)};
    vm.runInNewContext(source,{FileBackend,Worker,self,URL,PAGE_SIZE:65536,performance});
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
    assert.equal(h.workers.at(-1).commands[0].patches[0][0],4294967297n);
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
