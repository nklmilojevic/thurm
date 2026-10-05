//! A container whose children are placed by a callback (a tab's split layout).

use std::cell::RefCell;

use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

type Allocate = Box<dyn Fn(i32, i32)>;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct SplitBox {
        pub allocate: RefCell<Option<Allocate>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for SplitBox {
        const NAME: &'static str = "ThurmSplitBox";
        type Type = super::SplitBox;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for SplitBox {
        fn dispose(&self) {
            while let Some(c) = self.obj().first_child() {
                c.unparent();
            }
        }
    }

    impl WidgetImpl for SplitBox {
        fn measure(&self, _orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            (1, 1, -1, -1)
        }

        fn size_allocate(&self, width: i32, height: i32, _baseline: i32) {
            if let Some(f) = self.allocate.borrow().as_ref() {
                f(width, height);
            }
        }
    }
}

glib::wrapper! {
    pub struct SplitBox(ObjectSubclass<imp::SplitBox>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl SplitBox {
    pub fn new(allocate: impl Fn(i32, i32) + 'static) -> SplitBox {
        let b: SplitBox = glib::Object::new();
        *b.imp().allocate.borrow_mut() = Some(Box::new(allocate));
        b
    }
}
