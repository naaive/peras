//! `.tools((read, edit, Bash))`: tuples of tools.

use agent_runtime::Tool;
use std::sync::Arc;

pub trait IntoTool {
    fn into_tool(self) -> Arc<dyn Tool>;
}

impl<T: Tool + 'static> IntoTool for T {
    fn into_tool(self) -> Arc<dyn Tool> {
        Arc::new(self)
    }
}

pub trait IntoTools {
    fn into_tools(self) -> Vec<Arc<dyn Tool>>;
}

impl IntoTools for () {
    fn into_tools(self) -> Vec<Arc<dyn Tool>> {
        vec![]
    }
}

impl IntoTools for Vec<Arc<dyn Tool>> {
    fn into_tools(self) -> Vec<Arc<dyn Tool>> {
        self
    }
}

macro_rules! tuple_tools {
    ($($t:ident),+) => {
        impl<$($t: IntoTool),+> IntoTools for ($($t,)+) {
            #[allow(non_snake_case)]
            fn into_tools(self) -> Vec<Arc<dyn Tool>> {
                let ($($t,)+) = self;
                vec![$($t.into_tool()),+]
            }
        }
    };
}

tuple_tools!(A);
tuple_tools!(A, B);
tuple_tools!(A, B, C);
tuple_tools!(A, B, C, D);
tuple_tools!(A, B, C, D, E);
tuple_tools!(A, B, C, D, E, F);
tuple_tools!(A, B, C, D, E, F, G);
tuple_tools!(A, B, C, D, E, F, G, H);
tuple_tools!(A, B, C, D, E, F, G, H, I);
tuple_tools!(A, B, C, D, E, F, G, H, I, J);
tuple_tools!(A, B, C, D, E, F, G, H, I, J, K);
tuple_tools!(A, B, C, D, E, F, G, H, I, J, K, L);
