//! Read-only, original-fenced object-set enumeration for retained backed GC.
//! SQLite owns all its own descriptors; no raw DB File can release POSIX locks.
use super::*;
use std::sync::Arc;
pub(crate) struct RetainedObjectSetScan {store:LocalStore,original:Arc<dyn ImageImportFence>,connection:Option<Connection>,refs:Vec<ObjectRef>,ready:bool,attempted:bool}
impl RetainedObjectSetScan {
    pub(crate) fn capture_retaining(out:&mut Option<Self>,store:LocalStore,original:Arc<dyn ImageImportFence>)->Result<()> {
        ensure!(out.is_none(),"original object-set GC scan occupied");*out=Some(Self{store,original,connection:None,refs:Vec::new(),ready:false,attempted:false});out.as_mut().unwrap().run()
    }
    fn current(&self)->Result<()> {self.original.remaining()?;validate_presence(&self.store)?;
        if let Some(c)=&self.connection {let mut moved:libc::c_int=-1;let result=unsafe{rusqlite::ffi::sqlite3_file_control(c.handle(),c"main".as_ptr(),rusqlite::ffi::SQLITE_FCNTL_HAS_MOVED,(&mut moved as *mut libc::c_int).cast())};
            ensure!(result==rusqlite::ffi::SQLITE_OK&&moved==0&&Path::new(c.path().context("object-set SQLite filename absent")?).canonicalize()?==path(&self.store),"original object-set SQLite moved/unsupported");}
        self.original.remaining()?;Ok(())}
    fn run(&mut self)->Result<()> {
        ensure!(!self.attempted,"original object-set scan cannot retry");self.attempted=true;self.current()?;
        self.connection=Some(Connection::open_with_flags(path(&self.store),OpenFlags::SQLITE_OPEN_READ_ONLY|OpenFlags::SQLITE_OPEN_NO_MUTEX|OpenFlags::SQLITE_OPEN_NOFOLLOW)?);
        let clock=self.original.clone();
        {let c=self.connection.as_mut().unwrap();c.busy_timeout(Duration::ZERO)?;
            c.progress_handler(1000,Some(move||clock.remaining().is_err()))?;}
        self.current()?;let c=self.connection.as_ref().unwrap();
        // The enclosing actual store/GC lock excludes every supported writer.
        // A read transaction pins this original SQLite snapshot. Errors retain
        // the connection/transaction; there is no Drop mutation or ACK repair.
        c.execute_batch("BEGIN")?;schema(c)?;
        let integrity:String=c.query_row("PRAGMA quick_check",[],|row|row.get(0))?;ensure!(integrity=="ok","object-set corruption blocks backed GC");
        let mut statement=c.prepare("SELECT length(CAST(pin AS BLOB)),length(CAST(receipt AS BLOB)),length(CAST(refs AS BLOB)),pin,receipt,refs,count FROM object_sets ORDER BY pin")?;
        let mut rows=statement.query([])?;let mut all=BTreeSet::new();let mut records=0;
        while let Some(row)=rows.next()?{self.current()?;records+=1;ensure!(records<=MAX_RECORDS,"object-set GC record bound");
            let a:i64=row.get(0)?;let b:i64=row.get(1)?;let n:i64=row.get(2)?;
            ensure!((0..=4096).contains(&a)&&(0..=MAX_SET_JSON as i64).contains(&b)&&(0..=MAX_SET_JSON as i64).contains(&n),"object-set GC row bound");
            let name:String=row.get(3)?;let receipt:String=row.get(4)?;let refs:String=row.get(5)?;let count:i64=row.get(6)?;
            let(_,refs)=decode(&name,&receipt,&refs,count)?;all.extend(refs);ensure!(all.len()<=MAX_TOTAL_REFERENCES,"object-set GC total bound");
        }
        self.current()?;self.refs=all.into_iter().collect();self.ready=true;Ok(())
    }
    pub(crate) fn references(&self)->Result<&[ObjectRef]>{self.current()?;ensure!(self.ready,"original object-set scan incomplete");Ok(&self.refs)}
}
