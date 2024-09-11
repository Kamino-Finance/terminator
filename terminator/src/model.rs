use std::{
    cell::{Ref, RefCell, RefMut},
    rc::Rc,
};

use anchor_lang::prelude::Pubkey;
use kamino_lending::utils::AnyAccountLoader;

#[derive(Default, Clone)]
pub struct StateWithKey<T> {
    pub key: Pubkey,
    pub state: Rc<RefCell<T>>,
}

impl<T> StateWithKey<T> {
    pub fn new(state: T, key: Pubkey) -> Self {
        Self {
            key,
            state: Rc::new(RefCell::new(state)),
        }
    }
}

impl<T> AnyAccountLoader<'_, T> for StateWithKey<T> {
    fn get_mut(&self) -> anchor_lang::Result<RefMut<T>> {
        Ok(RefMut::map(self.state.borrow_mut(), |state| state))
    }
    fn get(&self) -> anchor_lang::Result<Ref<T>> {
        Ok(Ref::map(self.state.borrow(), |state| state))
    }

    fn get_pubkey(&self) -> Pubkey {
        self.key
    }
}
