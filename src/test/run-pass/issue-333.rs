fn quux<T: copy>(x: T) -> T { let f = bind id::<T>(_); ret f(x); }

fn id<T: copy>(x: T) -> T { ret x; }

fn main() { assert (quux(10) == 10); }
